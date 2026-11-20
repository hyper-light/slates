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
