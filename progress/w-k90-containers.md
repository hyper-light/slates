# w-k90-containers progress (kiln goal 90's container engines; Mac Studio; started 2026-11-19 ~22:00 PST)

POLICY_READ: briefs/w-k90-containers.md + common.md (incl. FAST LANDING, NO KILN, OWN TARGET, NEVER END, EVERY PIECE ENDS
ON origin/main); relaunch-2026-11-19.md completeness first; kiln AGENTS.md + CLAUDE.md (@AGENTS.md) at kiln f7ec15878.
Kiln's "person's word for every push" is given by Ada's rule via main (earlier owners pushed kiln main the same way).
Shards paths: none touched.
Setup: fanout-state read from a PRIVATE clone ~/.cache/w-k90-containers-fanout (other Studio workers race on
~/fanout-state, rm -rf'ing each other's clones). kiln clone ~/kiln-work/w-k90-containers-kiln; target
~/.cache/w-k90-containers-target.

## State at start (kiln main f7ec15878)
- On main: workspace::container::{start,end} (g293 piece 4), vendor/slates contract 48 (no Instance::export: K8s needs
  slates >= contract 50, A-521 -- a re-vendor, w-k90-vendor's queue item 1; coordinate).
- Not on main: Engine::Container, RunnerFacts.container, probe, harness launch through container::start. -> piece 5a first.

## Queue
1. (=3) A-520 piece 5a: runner facts + probe + engine selection for `container`; 5b: the launch through container::{start,end}.
2. K8s engine (W10 build order 2-6, g294's notes): needs the re-vendor at contract >= 50.

## Piece 5a (in progress, 2026-11-19 ~22:00 PST): a runner's container facts
- Split re-decided: 5a = facts only (Engine::Container would be selectable but unlaunchable without 5b); 5b = engine
  (held from facts, set/asked_of, placement/drain, session marker) + launch through workspace::container.
- Uncommitted in ~/kiln-work/w-k90-containers-kiln (from main f7ec15878): kiln-ci facts.rs ContainerFacts +
  ContainerMissing{workspaces,engine,profile,shared_mount,platform} + RunnerFacts.containers; workspace/container.rs
  facts()/facts_of()/engine_word() + test w9_a_runners_facts_say_whether_its_agents_can_run_in_containers_and_what_it_lacks;
  runner/probe.rs one line; docs/agentic.md W9 + status row. Committed e73ad6d39 (branch agents/w-k90-containers-5a, local).
- Test (macOS arm64, this Mac, local): `cargo test --locked -p kiln --lib -- workspace::container wire::ts` 14 passed incl.
  the new w9 test and bindings_are_current. Merged origin/main (19c0e4160; config.rs, slates_fleet*); re-run in flight.
- LANDING ROUTE (main 2026-11-19 ~23:50): this Studio has no GitHub creds; `kiln run` on adas-mac-studio-2.local
  (b24c5c951260dcdfd0972f2aeaf3cf51), cwd = this worktree (synced), `git push git@github.com:hyper-light/kiln.git
  HEAD:main`, then is-ancestor; retry if main moved. State branch: same, from ~/fanout-state-w-k90-containers.
  First sync ~670 MB took ~10 min, and main moved meanwhile (push r2 rejected).

## 5b plan (decided by me, tell main): launch first, engine second
- A container session must NOT reuse harness Isolation (104 sites read isolation.is_some() as microVM): own marker.
- 5b-i: sandbox::container implementing kiln-seams Boundary::Container{image} over workspace::container (keeper
  container + `<cli> exec -i -w <workdir>`), unit-tested over stand-ins, unreachable until 5b-ii.
- 5b-ii: Engine::Container (appended; serde "container"), held from RunnerFacts.containers, asked_of/set (409 when
  not available), split/drained by session engine, lineage wanted/on_engine, exhaustive matches; image source TBD
  (Agentfiles are Shards images built on microvm runners only).

## Piece 5a: LANDED 2026-11-20 ~00:5xZ, kiln main ..edc8a2c8e (e73ad6d39 + merges 2b0cdc815/19c0e4160/.../edc8a2c8e)
- is-ancestor confirmed from the Studio. Merges: config.rs/slates_fleet (re-ran 14/14), review.rs/git.rs (w-k90-vendor
  filemode), docs/agentic.md status-row conflict (main's row + my W9 sentence). Reported to main (agents_send).
- NEXT: 5b-i (sandbox::container), then 5b-ii (Engine::Container).

## 5b-i (2026-11-20 ~01:xxZ): committed a69822973 on agents/w-k90-containers-5a (worktree ~/kiln-work/w-k90-containers-kiln)
- sandbox::container::ContainerSandbox (keeper `tail -f /dev/null` + engine `exec --interactive`, env by name only,
  release on end/kill), workspace::container::{release, Plan.entrypoint}; 4 new tests; 11/11 locally.
- Merge 7e77ebecd brought main's re-vendor (Slates 38ed3dfd8, CONTRACT 50: Instance::export now vendored) -> re-run in
  flight (target ~/.cache/w-k90-containers-target). Then push via adas-mac-studio-2.
## K8s builders (main asked ~01:xxZ): worktree ~/kiln-work/w-k90-containers-k8s (branch agents/w-k90-containers-k8s
  from 7e77ebecd), target ~/.cache/w-k90-containers-target-k8s: crates/kiln/src/k8s/volume.rs (objects(): PV nfs
  xprtsec=mtls/claimRef/Retain, claim volumeName "", pod volume+mount+runAsUser/Group; PodExport Debug hides token;
  Unbuildable), 2 w10 tests; W10 doc line. RBAC deferred to the engine piece (first code that creates PVs). Run in flight.
- Next K8s piece: kiln's call of Instance::export (workspace::slates::export) + the engine/home + RBAC.

## 5b-i LANDED 2026-11-20 ~02:0xZ kiln main ..7e77ebecd (a69822973); reported to main.
## K8s builders: committed d5d613ef6 (worktree w-k90-containers-k8s), 2/2 green; push run r8 (b24c) in flight.
## 5b-ii (in progress, worktree ~/kiln-work/w-k90-containers-kiln, uncommitted):
- Engine::Container (kiln-ci facts), [agent] container_image (runner::probe::Declared), ContainerFacts.image +
  ContainerMissing::Image; harness::engine: containers(), held(asked, can, boxed) (never container by default), asked_of
  "container", set 409 unless containers usable, session_engine()/split/drained, view "container"/"containers";
  session::InContainer + Spec/Info.container (copied at host.rs/registry.rs/session.rs; Info::differs);
  lineage::container_at + on_engine arms; registry::open_as sets spec.container for new non-isolated conversations;
  narrow::Whole::Container; sandbox::container::{launch_of, launch}; session launch branch (no MCP, no kind.prepare);
  experiments' matches (Container grouped with Host). Docs W9 + status row.
- Tests added: engine w9_a_runner_holds_container_only_..., a_put_sets_host_microvm_container_or_null...,
  lineage w9_a_container_runners_new_conversations_take_its_image..., sandbox::container
  w9_a_sessions_container_launch_is_its_harness_by_name..., workspace::container facts test (image).
- Run t1 in flight (KILN_WRITE_TS=1).
- Gap to report: a harness in a container has no sign-in (image's own or a credential grant: next piece).

## 5b-ii LANDED 2026-11-20 ~02:4xZ kiln main 4eda3a697 (fast-forward; is-ancestor confirmed); reported to main.
- NOT done (told main): browser check of the Runners page (Chromium/WebKit); live container start.
- ui/node_modules installed in ~/kiln-work/w-k90-containers-kiln/ui (npm ci) for UI checks.
## K8s builders d5d613ef6: push r8 still running on b24c (slow first sync of the k8s worktree path).
## NEXT: W10 engine scoping (export call over the link, PV/claim/pod creation, RBAC), W9 sign-in in container.

## LANDED 2026-11-20: K8s builders kiln main 948ca0a57 (d5d613ef6); W9 .nfs.* silly-rename exclusion 3046fed87.
- r8 (worktree sync) hung >1h: git worktrees sync badly via kiln run -> land everything from ONE clone
  (~/kiln-work/w-k90-containers-kiln); k8s worktree + its target removed.
- Reported not built: Docker Desktop share check (settings key unverified; no Desktop on this Studio).
## NEXT: W10 home holder: workspace::Ask::{Hold, Release} (no proto change), workspace/held.rs map keyed by pod
   session id, slates::export wrapper (Instance::export), loops::workspace falls back to held map; then server side.

## W10 home holder LANDED 2026-11-20 kiln main fce36c347 (workspace::held, Ask::{Hold,Release}, feature workspace.hold).
## NEXT: W10 server side: pod start -> hold at home -> host/port from [slates_fleet.nodes] -> k8s::volume::objects ->
   kubeapi create PV/claim/pod (new verbs) + chart RBAC (Role pvc create/delete in jobs ns; ClusterRole PV create/get/delete);
   end/reaper: delete pod, claim, PV; release at home.

## W10 cluster piece LANDED 2026-11-20 kiln main 8c501c906 (kubeapi PV/claim verbs, k8s::volume::{make,unmake}, chart rbac.agentPods).
## ANSWERED by main 2026-11-20 (asked ~00:4x PST; both built since: 82e81809f, 3a7e4116c..75757b07d):
- W10 pod start: ANSWERED -- ephemeral one-session runner in the pod (built: 3a7e4116c, 5c5adfae9, b07098718, 75757b07d).
- W9 harness sign-in in a container: ANSWERED -- credential grant by name (built: 82e81809f).
## Owed by others: live container start + Runners page browser check (main); Docker Desktop share key (needs a Mac with
   Desktop); Windows (jupiter BLOCKED).

## main's decisions 2026-11-20: W10 = ephemeral one-session runner in the pod; W9 sign-in = named secret granted per
   session; jupiter unblocked; do the Runners browser check myself; queue the live start on adacachy.
- LANDED 68964a93d: Runners page browser check (e2e/runner-engine-container.spec.ts, 4/4 Chromium+WebKit desk+phone).
- LANDED 82e81809f: W9 sign-in via Kind::in_container (environment secrets by name into a container's exec).
- Live start: run r19 (linux/amd64, cpus=4, mem=32g) building + running workspace::container_live_tests (file
  uncommitted in the clone: crates/kiln/src/workspace/container_live_tests.rs + mod.rs line); commit it when r19 reports.
## NEXT: W10 ephemeral one-session runner (`kiln agent --ephemeral --session`), pod start + reaper; then Windows items.

## 2026-11-20 later: LANDED 8728d0807 BUILD FIX (sandbox::container was cfg(test) since a69822973 -> non-test build broken
   4eda3a697..8728d0807; told main), 3a7e4116c (kiln agent --ephemeral --session), a3d2d7edf (k8s::volume::agent_pod).
- RULE for me now: `cargo check --locked -p kiln --bins` before every landing (lib tests run cfg(test) only).
- r19 (live container test on linux) still queued (adacachy under strain); it syncs at start (fixed tree + uncommitted
  container_live_tests.rs).
## NEXT: W10 (b) the pod start: lineage `isolation: {"kind":"pod"}`/engine? -> hold at home (Ask::Hold via ask_runner) ->
   [slates_fleet.nodes] host/port -> objects -> make -> create agent_pod with a one-time token -> on the pod runner's
   connect open the session there (cwd = folder; review/drift/landing asked of the home by session id) ; (c) reaper.

## LANDED 2026-11-20: 5c5adfae9 k8s::agent::AgentPods; b07098718 K8s::start_agent (+[kubernetes] agents_image);
   75757b07d agents.start isolation "pod" + reaper. W10 code path complete; KIND proof is main's.
- Reported: k8s::tests::w02_r06_kilns_k8s_jobs_get_v1s_pods_on_a_k8s_pool fails on main since aa98b33d6 (not mine).
- r24 (live W9 test on adacachy, rootful docker 29.9.0 only) queued; container_live_tests.rs still UNCOMMITTED
  (+ workspace/mod.rs line) -> commit after r24 reports.
## NEXT: Windows items (jupiter unblocked): W9 measurement (Docker Desktop WSL2 9P reaching a WinFsp drive) / W5-ish.

## LANDED 2026-11-20: 234f8c211 (stale k8s test w02_r06 fixed: ci.yaml:rust on bare metal since aa98b33d6),
   2b5a3d559 (k8s node tlshd cert: POST /api/kubernetes/node-certificates, kind k8s-node refused on link).
## W10 code done except the KIND proof (main's). Open: r24 live W9 (adacachy), r30 Windows cargo check (jupiter),
   W9 Windows measurement (person: WinFsp, console session, Docker Desktop). container_live_tests.rs uncommitted.

## STATUS 2026-11-20: nothing waits on main. Waiting: r24 (live W9 on adacachy, load 74), r33 (Windows cargo check,
   queued for jupiter). Blocked on a PERSON only: W9 Windows measurement (WinFsp, console-session runner, Docker Desktop).

## 2026-11-20: Windows `cargo check -p kiln --bins --tests` at 2b5a3d559 PASSED on jupiter (r33). r24 failed on adacachy /tmp full (my TMPDIR mistake) -> r36 queued with TMPDIR under ~/.cache.

## RULE (Ada 2026-11-20, common.md): fetch + merge origin/main at each piece start, before each reported test run, every 30 min; name the base (origin/main short hash) with every result; delete landed worktrees by exact path.

## 2026-11-20 ~08:xx live W9 (main: drive it): r24/r36 crawled on adacachy (load ~70, rootful docker -> would only skip)
   -> stopped. Live run on main's MacBook (e30189ea) with Docker Desktop 29.3.1 (started by me, quit after): binaries
   built here (base ed8991d29), served to it over LAN (python http.server on 192.168.1.82:18497, my task), run from
   ~/.cache/w-k90-containers-live there.
- r52 RED: "this process keeps no workspaces" (test lacked slates::init) -> test fixed (state under ~/.cache = /Users share).
- r54 RED (real bug): container ran on the bind, read+wrote the volume, went; then the mount stayed BUSY, held by
  com.apple.Virtualization.VirtualMachine (Docker Desktop's share). Cleaned up by hand (quit Docker, umount -f, anchor gone).
- Fix (uncommitted): slates::remove_after_container (let go of a busy mount once container+bind are gone), used by
  workspace::container end + workspace::release for container sessions + the live test; unit test w9_a_containers_busy_...
  11/11 local; bins check clean. r61 = green attempt.

## LANDED 2026-11-20 fa86489f5 (on origin/main 09ea9b826): container end lets a busy workspace mount go
   (slates::remove_after_container) + workspace::container_live_tests. LIVE GREEN r70: macOS arm64 Adas-MacBook-Pro-2,
   Docker Desktop 29.3.1, alpine:3.22, binaries at base ed8991d29 + piece: 1 passed 10.99 s, mounts left 0 (red r54 before
   the fix: mount busy, held by com.apple.Virtualization.VirtualMachine). Unit on merged tree (base 6965eaf63): 13/13.
   NOTE: fa86489f5's message says "merged on base 6965eaf63, 11 passed" -- that run was actually on ed8991d29; the
   merged run is 13/13 above.
## Ada's ruling 09:30: container engine moves to a `local` nfs Docker volume of Slates' export (owner w-k90-a520);
   bind route goes after its A/B. I don't edit sandbox::container / workspace::container. Hand-offs written:
   inbox/w-k90-a520-from-w-k90-containers.md (+ agents_send), inbox/w-k90-win-from-w-k90-containers.md (r33 findings).
## MINE NEXT: proof by use of the OCI/Docker engine through the NEW route on Linux (adacachy), macOS (Docker Desktop),
   Windows (jupiter, once w-k90-win's build + WinFsp) -- after w-k90-a520 lands the route.
