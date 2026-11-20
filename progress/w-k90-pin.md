# w-k90-pin progress (kiln goal 90 vendoring part 01a1ec34599974448569750ac79093b6; brief briefs/w-k90-pin.md)
This Mac (adalundhe). State clone ~/fanout-state-w-k90-pin. Kiln clone ~/kiln-work/w-k90-pin-kiln (blobless).
Windows half is w-k90-win's (inbox/w-k90-win-from-w-k90-pin.md).

## Audit (kiln origin/main 6965eaf63, 2026-11-20)
1. Pin: HOLDS. vendor/MANIFEST.toml [projects.slates] commit 38ed3dfd8e69 (git, take/own lists), vendor/slates/UPSTREAM.md;
   checked by .kiln/ci.yaml job `vendor` (`scripts/vendor.py check --kind git`), green in ci run
   01a1f734930b762286bc66a7ec300a10 (kiln main, 2026-11-19). Root Cargo.toml patches slates-api/slates-ipc to
   vendor/slates; tests/vendored.rs holds one revision. Local re-check 2026-11-20 (this Mac): `vendor.py check slates`:
   1355 files at 38ed3dfd, 2 recorded changes, 0 differences.
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
- main 09:55: Windows arm64 follows from the platform rule; cross `cargo check --target aarch64-pc-windows-msvc`
  (Slates' cross-lint way) since kiln reaches no windows-11-arm. kiln fb5062a85 (branch): job
  slates-windows-arm64-cross (target added inside a linux/arm64 container; C-free crates slates-wire/-mem/-rt/-ipc/
  -merge/-db/-wire-derive at kiln's lock). Dispatched on the branch.
- Local macOS arm64 lane (~/kiln-work/w-k90-pin-logs/lane.sh, target ~/.cache/w-k90-pin-target.noindex, debug 0):
  build ok; tests compiling. Land 2898dfe42 (+fb5062a85) on its green, merge-base confirmed.
- Clause 2 sweep (docs, README, deploy, scripts, ci, Dockerfile, src, ui/src, .kiln): only agentic.md 717/877 name
  Slates' own CLI beside the library call kiln makes; no instruction to run it. Holds.

## 2026-11-20 ~10:40 PST: audit CORRECTED
- kiln's GitHub Actions run (public repo; api.github.com readable without a credential). kiln's server's "GitHub
  workflows are turned off" only means it doesn't dispatch them. ci.yml `build` matrix = macos-latest, windows-latest,
  windows-11-arm, ubuntu-24.04-arm (clippy + build); `linux` tests; w-k90-win's `windows-slates` tests on Windows x64.
  Run 44758164143 (ed8991d29): Clippy failed on every lane, so Build never ran: no platform was green.
- Dropped: branch agents/w-k90-pin-ci (2898dfe42, fb5062a85: .kiln lanes; 2898dfe42's message wrongly says GitHub
  CI doesn't run) -- not landed. Its trial runs 01a1f9fb0653..., 01a1fa1643f3... are queued; cancel needs a person's
  click (for main).
- Landing kiln 7751fa96d (worktree ~/kiln-work/w-k90-pin-land, base 0a17f46e5): ci.yml Build runs whatever Clippy
  says + "Slates tests" step on macos-latest/windows-11-arm/ubuntu-24.04-arm; linux job tests whatever Clippy says;
  slates_roles compile fix (Made.shared, E0063 since 6965eaf63); UI mock.
- Local macOS lane 2 (lane2.sh, log lane2-macos.log) on the merged tree: running.
