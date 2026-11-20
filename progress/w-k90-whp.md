# w-k90-whp progress (kiln goal 90: Shards microVM drive proven through kiln on Windows WHP; worker on Ada's MacBook Pro)
Started 2026-11-20 ~08:30 PST. Read: briefs/w-k90-whp.md, briefs/common.md, relaunch-2026-11-19.md (state branch copy),
progress/w-k90-microvm.md, progress/w-k90-kvm.md, g262's W8 notes, run-requests (W8 live run 2).

## Setup
- State folder ~/fanout-state-w-k90-whp (SSH push). Kiln clone ~/kiln-work/w-k90-whp (blobless, origin/main ed8991d29).

## Findings (2026-11-20)
- kiln main has W8 pieces 1, 2 only (docs/agentic.md "W8 on Linux KVM and Windows WHP"): piece 3 (Windows known
  holder: SlatesVolumes keeps the attached handle until the box's run ends), 4 (/api/harness/workspaces says served
  volumes), 5 (microvm:: drive opened to Windows; `#[cfg(unix)] mod microvm`) are NOT built. The WHP drive can't
  compile on Windows until 5.
- kiln's own CI for hyper-light/kiln: providers = kiln only ("GitHub workflows are turned off for this repository" in
  kiln). GitHub itself still runs kiln's .github/workflows/ci.yml on every push (gh run list), so a kiln workflow on
  GitHub's windows-2025 is possible. Shards' own CI runs WHP partitions there (ci.yml SHARDS_REQUIRE_VMS=1 on
  windows-2025/2022; whp-run.yml probes, M200 an AppContainer partition), so a guest can boot there.
- jupiter can't pack Shards without installs: pack.sh downloads NASM and zig into ~/.cache when missing, and Shards'
  guests need the musl rustup target. All three are tool installs (not authorized).

## Plan
1. Pieces 5 (+3, +4 as the drive needs) in kiln, red first on Windows.
2. A kiln .github workflow (workflow_dispatch) on windows-2025: pack Shards, run `SLATES_E2E=1 VM_E2E=1 slates_e2e microvm::`.

## Piece A: W8 piece 5 (drive opened to Windows) + the WHP run workflow -- BUILT, on branch, not on main yet
- kiln commit 7aac3c730 (base origin/main ed8991d29), pushed as kiln branch agents/w-k90-whp:
  tests/slates_e2e.rs `mod microvm` cfg(any(unix, windows)) (g90 resync drive stays unix); `host_mounts` on Windows lists
  drive letters holding Slates' FS (`SLATES_DRIVE`), `session_mounts` takes each for the session's (unit test extended);
  .github/workflows/whp-drive.yml (windows-2025, SLATES_E2E=1 VM_E2E=1 microvm::, asserts a g266 `... ok` line);
  docs/agentic.md W8 pieces 5/6.
- Runs: GitHub 44773050371 (whp drive, windows-2025, on 7aac3c730) started 16:24Z. kiln r1 (fmt check + the focused
  unit test, KILN_SHARDS=lint) queued: no runner with cargo had room (this MacBook has no Rust).
- Next: land on main once r1 is green (fmt + unit); read 44773050371's red/green; fix what it shows (pieces 3/4 if
  reproduced: EACCES on DAX reads = piece 3; "gives agents no Slates volume" SKIP-turned-failure = piece 4).

## Piece B: W8 piece 4 (served volume said by /api/harness/workspaces) -- WRITTEN, uncommitted in ~/kiln-work/w-k90-whp
- Why: windows-2025 has no WinFsp, so the drive's `workspaces_ready` (mounted judgment) fails under SLATES_E2E=1
  before any microVM starts (expected red; awaiting 44773050371 to show it).
- api.rs `workspaces_answer` + `served` per target (WorkspaceUse::Served); test api::tests::w8_p4_...; slates_e2e
  `workspaces_said/ready(.., Engine)` read `served` for MicroVm; ui WorkspacesAvailable type; docs W8 piece 4.
- r1/r2 never placed (Mac links down); r3 (adas-macbook-pro-2, fmt + 2 slates_e2e checks + lib w8_p4_, KILN_SHARDS=lint)
  running ~17:20Z.

## LANDED 2026-11-20 ~09:32 PST on kiln main (merge-base checked): 7aac3c730 (piece 5 + whp-drive.yml), 4f9992f42
(piece 4), merge 72e6daa92 over main 842cc2c2f, ac1a78f64 (ci.yml Windows Slates job --skip microvm::), 0a17f46e5 (YAML
fix: ac1a78f64's plain scalar broke ci.yml parse ~1 min: GitHub runs 44784109897, 44784124403 failed at 0 s).
- Red that drove piece 4: GitHub 44773050371 (7aac3c730, windows-2025, HypervisorPresent True): drive compiled and ran
  (1 test; 31 min build), runner said microVMs yes, then failed "gives agents no Slates volume of their own: ... WinFsp is
  not installed (SLATES_E2E=1 requires one)". Log ~/.cache/w-k90-whp-logs/gh-44773050371.log (this MacBook).
- Green focused tests: kiln r5 (adas-mac-studio-3, clone of 4f9992f42, KILN_SHARDS=lint): checks::g266_a_sessions_host_mounts...,
  checks::g266_kilns_word_on_a_runners_workspaces... 2/2; lib api::tests::w8_p4_... 1/1. kiln AGENTS.md: no tree-wide
  cargo fmt (clippy is the check); clippy on 3 OSes owed (main's gate).
- Running: GitHub whp drive 44784112194 on main ac1a78f64. Next: read it; piece 3 if DAX reads fail EACCES.
- Inbox note left for w-k90-win (its ci.yml step changed).

## For main to delete
- (none)
