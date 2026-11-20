# w-k90-pin progress (kiln goal 90 vendoring part 01a1ec34599974448569750ac79093b6; brief briefs/w-k90-pin.md)
This Mac (adalundhe). State clone ~/fanout-state-w-k90-pin. Kiln clone ~/kiln-work/w-k90-pin-kiln (blobless).
Windows half is w-k90-win's (inbox/w-k90-win-from-w-k90-pin.md).

## Audit (kiln origin/main 6965eaf63, 2026-11-20)
1. Pin: HOLDS. vendor/MANIFEST.toml [projects.slates] commit 38ed3dfd8e69 (git, take/own lists), vendor/slates/UPSTREAM.md;
   checked by .kiln/ci.yaml job `vendor` (`scripts/vendor.py check --kind git`), green in ci run
   01a1f734930b762286bc66a7ec300a10 (kiln main, 2026-11-19). Root Cargo.toml patches slates-api/slates-ipc to
   vendor/slates; tests/vendored.rs holds one revision.
2. No CLI: HOLDS in code. crates/kiln uses slates_api only; anchor/daemon = kiln's own exe (`kiln slates-role`,
   slates_api::role::main); tests/slates_roles.rs asserts no `slates` binary runs; slates_e2e's CLI const is only
   used to assert absence. workspace/slates.rs is the library port (no Command for slates; Commands only for git,
   lsof, systemd-run). Leftover found: ui/mock/agents.ts told a person to run `slates grant` -> fixed (below).
   Docs mentioning `slates oci-runtime`/`slates export` describe Slates' own CLI, not a kiln step (left).
3. CI every platform: NOT MET. .github/workflows/ci.yml has the mac/windows/arm matrix but kiln's GitHub workflows
   are turned off (ci.workflows.list: "GitHub workflows are turned off for this repository"; provider = kiln only).
   .kiln/ci.yaml builds+tests on Linux x86-64 alone (rust job; not green on main recently: timeouts/failures, main's).
   Runners: macOS arm64 (several), Linux x86-64 (451397), Windows x86-64 (jupiter f766); NO Linux arm64, NO Windows arm64.
   vendor/slates has no Cargo.lock (kiln's stands for it), so slates' own crate tests can't run --locked in kiln;
   the lanes test kiln's use of it.

## Fixes
- kiln 2898dfe42 on branch agents/w-k90-pin-ci (base 6965eaf63): .kiln/ci.yaml jobs slates-linux-x86_64,
  slates-macos-arm64, slates-linux-arm64 (linux/arm64 container on an Apple-silicon runner's Docker, 31-62 GiB VMs):
  build kiln + `--lib workspace::slates`, `--test vendored --test slates_roles`. UI mock text fixed.
  Validated 0 errors. Trial run 01a1f9fb06537697bf01eb3419a51470 (jobs filter, queued 'headroom').
- NEXT: land on green; then Windows x86-64 lane (w-k90-win) and Windows arm64 (no runner: main/Ada).
