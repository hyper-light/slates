# w-k90-whp: goal 90's microVM proof on Windows WHP (main, 2026-11-20 ~09:00 PST)

## Why
Ada (2026-11-20, repeated): spawn agents for kiln goal 90's remainders; "It needs to be completed." The microVM part
is proven on macOS HVF; w-k90-kvm owns Linux KVM; nobody owned Windows WHP.

## Your part (kiln goal 28e73c34; main marks it met from your evidence)
The WHP half of part 01a1f1249c5e75b5bc82c9eb011550f4: "kiln's Shards microVM engine proven through kiln on Linux KVM
and Windows WHP as well as macOS HVF". The proof is kiln's own drive (`SLATES_E2E=1 VM_E2E=1 slates_e2e microvm::`,
which passes 2 of 2 on HVF at kiln main 81174ac49) run through kiln on Windows, red first for whatever fails.

## What's known
- WHP works on jupiter, measured 2026-10-26: Shards' M197 probe printed hypervisor_present=1 and ran partitions.
  `Get-WindowsOptionalFeature HypervisorPlatform` reads "Disabled" there and is misleading; don't trust it.
  Read-only, 2026-11-20: Hyper-V on, HypervisorPresent=True, C: had 7 GB free.
- Real `shards run` guests make an AppContainer profile per VM (Shards D141). Runs on jupiter have avoided that
  (M343 ran --plain), and full guest proofs went to GitHub's windows-2025 runners. Main has asked Ada whether
  AppContainer profiles may be made on jupiter. Until she answers, find out whether kiln's CI can run the drive on a
  GitHub windows-2025 runner, and get everything else ready on jupiter (kiln building on Windows, the drive compiling,
  every step short of starting a guest).
- kiln building on Windows is w-k90-win's part. Work with it through inbox notes on the state branch. Don't build a
  second copy on jupiter while its disk is this tight.
- No NASM on jupiter: for a scratch TEST build that pulls aws-lc set AWS_LC_SYS_PREBUILT_NASM=1 in your own
  environment and say so in the commit. jupiter has no musl target either: for a test build give
  SHARDS_INIT_BINARY/SHARDS_SERVER_BINARY a stand-in file. Installing NASM or a rustup target is a tool install: not
  authorized.
- w-k90-microvm's notes (`git show origin/agents/fanout-state:progress/w-k90-microvm.md`) and w-k90-kvm's (KVM defects:
  a guest member's exec failing; a session whose member never started is never ended) are worth reading first.

## Shards policy (binding if you touch Shards)
If the proof needs a change in Shards (hyper-light/shards, kiln's vendor/shards or vendor/patches/shards), that is
Shards work. Before any edit, read Shards' current origin/main AGENTS.md and docs/development-rules.md in full (git
show), follow R01-R16 and docs/development-evidence.md, record the policy revision/hash and your role (implementer),
and read the policy back to main in your next report. Under that policy, profiling, benchmarking, audits, heavy real
E2E, chaos and sweeps of Shards are the coordinator's; you do scoped implementation and light tests. Shards' macOS
support is arm64 only.

## Rules
Follow ~/fanout-state-w-k90-whp/briefs/common.md in full (clone the state branch first:
`git clone --branch agents/fanout-state git@github.com:hyper-light/slates.git ~/fanout-state-w-k90-whp`). Kiln work
lands on kiln's origin/main, confirmed with merge-base. Never force. Never checkout, switch, stash, reset or
commit-tree. No admin, installs or system changes without Ada's word (ask main). Never print a credential. Bound every
run and read its output at least every 10 minutes. Keep ~/fanout-state-w-k90-whp/progress/w-k90-whp.md current and
pushed. Report per part: landed (commit), ran (run id, numbers, base commit), next.
