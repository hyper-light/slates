# w-k90-containers -> w-k90-a520: sandbox::container and workspace::container, for the export route (2026-11-20)

I won't edit these files while you replace the bind with the export. What's there (kiln main 0a17f46e5 + fa86489f5):

workspace::container (crates/kiln/src/workspace/container.rs)
- start(made, plan) = identity_options (per Slates' TestedProfile identity rule; 5c added shared/rootful) -> Steps::bind
  (Slates' Instance::bind_into_container) -> engine_mount (Slates' OciBinding -> `--mount` words; nosuid/nodev held by
  --security-opt=no-new-privileges, --cap-drop=MKNOD) -> Steps::check (ContainerBind::check_source just before run) ->
  Steps::run (`<cli> run --detach --name .. --mount .. [--entrypoint] --workdir <same path> <image> <cmd>`).
  Refused after the bind: the bind is detached. Plan.entrypoint exists so a keeper runs instead of the image's own.
- release(running, made) = container rm --force, then bind detach; end() = release + Steps::take_down.
- take_down (Machine) = slates::remove_after_container: W3 remove, and if the mount stays BUSY, Slates' unmount_busy
  then remove again. MEASURED LIVE (fa86489f5, r54 red/r70 green, Docker Desktop 29.3.1 on a Mac): Docker Desktop's
  VM holds files a container touched past its life, so the plain unmount is refused busy. With the export route the
  host mount may not exist at all; then this goes away, but whatever holds an NFS client mount inside Docker's VM
  (the `local` nfs volume) should be removed with `docker volume rm` after the container, before the export detaches.
- facts()/facts_of(): runner container facts (RunnerFacts.containers): workspaces -> [agent] container_image ->
  runner::docker::find -> slates engine_profile -> identity rule. ContainerMissing::{Workspaces, Engine, Profile,
  SharedMount, Platform, Image}. With the export route, SharedMount and (on Windows) Platform likely change meaning.
- Tests: Steps stand-ins (Stand) hold the order; w9_* names.

sandbox::container (crates/kiln/src/sandbox/container.rs)
- ContainerSandbox{program, judged, made}: plan_of (Boundary::Container{image}, HOLDS: no limits/signals/shares,
  folder must be inside made.mount by components) -> workspace::container::start with keeper `tail -f /dev/null` ->
  harness via `<cli> exec --interactive --workdir <folder> --env NAME... <id> <program> <args>` (values only in the CLI's
  own env, never on the command line) -> ContainerRun releases (container+bind) on Exited or kill.
- launch_of(kind, spec, image) uses Kind::in_container (environment secrets granted by name allowed, unlike microVMs);
  launch(kind, spec, image) needs spec.workspace volume with a MOUNTED made (made.mount non-empty) -- with the export
  route a container session's workspace could be made unmounted (as an isolated one's), so this check changes.
- Session hook: harness/session.rs `None if spec.container.is_some()` -> sandbox::container::launch; no kiln MCP;
  no kind.prepare. Placement: lineage::container_at (image from facts), registry::open_as sets Spec::container.

Live test: crates/kiln/src/workspace/container_live_tests.rs (KILN_TEST_KILN + KILN_TEST_CONTAINER_IMAGE, skips with the
reason elsewhere): make+mount a volume (slates::init under ~/.cache so Docker Desktop's /Users share sees it), run
alpine `sh -c 'cat seed.txt && echo made-here > from-container.txt'` via ContainerSandbox, check output, the file on the
mount, the container gone, then remove_after_container. It's the natural A side of your A/B; the B side swaps make()
for an unmounted volume + export and the run for a `local` nfs volume. How I ran it on a Mac with Docker Desktop:
build kiln + `cargo test --no-run` on a Studio, zstd both, serve over LAN (kiln run sync was ~80 KB/s), run the one
test --exact on the Docker Desktop Mac, quit Docker Desktop after; on adacachy set TMPDIR under ~/.cache (/tmp is a
shared RAM disk: my build filled it once).
