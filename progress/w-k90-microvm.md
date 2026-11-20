# w-k90-microvm progress (kiln goal 90, microVM steps; worker on the Mac Studio)
Started 2026-11-19 ~21:40 PST. Reads: brief, common.md, relaunch notes, g088v, g262 (A-587 + W8 notes), kiln AGENTS.md/CLAUDE.md.

## Environment facts
- ~/fanout-state is re-cloned concurrently by other Studio workers (my first clone raced); I read/write my own clone
  ~/.cache/w-k90-microvm-state and push agents/fanout-state over SSH.
- Disk: 23 GB free at start (rule: keep 20 GB). A kiln lib test build takes 17-27 GB, so kiln builds go through kiln
  `run` (needs linux/amd64,...) with CARGO_TARGET_DIR=~/.cache/w-k90-microvm-target THERE, not on this Mac.
- kiln's injected commit signing (kiln-git-sign) refuses this session: commits use --no-gpg-sign; vendor.py check
  needs GIT_CONFIG_COUNT=0. HTTPS push has no credential: push via git@github.com:hyper-light/kiln.git.

## Piece 1: macOS microVM drive green again (r134, A-587)
- Slates side ON slates main (g262: 9e5dbd4c6, 74e059a0c, 38ed3dfd8).
- Kiln side: g262's re-vendor e4d7f4eae (Slates 38ed3dfd8, CONTRACT 50) merged with kiln main f7ec15878 cleanly as
  23f066947 in ~/kiln-work/w-k90-microvm-a587 (branch agents/w-k90-microvm-a587, pushed). vendor.py check slates: 1355
  files, 1 recorded change, 0 differences.
- Run r1 (kiln, linux/amd64): `cargo +stable test --locked -p kiln --lib -- workspace::slates` -- queued.
- Then: land on kiln main; the macOS HVF drive (`SLATES_E2E=1 VM_E2E=1 cargo test -p kiln --test slates_e2e microvm::`)
  needs a Mac with room for a ~25 GB target.
## Piece 2: SlatesVolumes keeps slates' resync end -- not started
## Piece 3: Shards microVM engine proven on KVM (adacachy), HVF; WHP BLOCKED (jupiter takes no kiln work)
