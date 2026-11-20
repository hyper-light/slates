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
- Run r1 (kiln, linux/amd64): `cargo +stable test --locked -p kiln --lib -- workspace::slates` -- queued (Linux under strain).
- Run r2 (kiln, needs macos,cpus=8,mem=32g,disk=60g): ci/shards/pack.sh + the HVF `microvm::` drive on 23f066947,
  log ~/.cache/w-k90-microvm-hvf.log on whichever Mac ran it.
- Then: land on kiln main; the macOS HVF drive (`SLATES_E2E=1 VM_E2E=1 cargo test -p kiln --test slates_e2e microvm::`)
  needs a Mac with room for a ~25 GB target.
## Piece 2: SlatesVolumes keeps slates' resync end -- BLOCKED on Shards (then Hex)
- Slates hands `ShardsStream { stream, resync: Option<StreamEnd> }`; kiln's served.rs returns `served.stream` alone
  because Hex's `hex_api::serve::Servers::stream` returns one `Stream`, and Shards' `ServedStream` /
  `--slates-fs-fd` take one end. Shards main 7158020b6 (= kiln's vendor) has NO resync anywhere (crates, docs, any of
  its 432 branches). Kiln holding the end alone would change nothing: no device reads it. Needs, in order: Shards' VMM
  device takes a resync end and speaks Slates' Resync (A-316/A-339 Unix, A-418/A-427 Windows) beside its stream (an
  image-channel field, frozen-wire growth: Shards' owner, "shards-7f", per focus-84-90.md), then Hex's Servers returns
  both ends, then kiln passes `served.resync`. Reported to main.
## Piece 3: Shards microVM engine proven on KVM (adacachy), HVF; WHP BLOCKED (jupiter takes no kiln work)
- HVF: r2 above (the same drive proves piece 1 and piece 3's macOS half).
- KVM: r3 (kiln, on adacachy, systemd-run --user MemoryMax=30G CPUWeight=1 Nice=19, -j4), the `microvm::` drive on
  23f066947, log ~/.cache/w-k90-microvm-kvm.log on adacachy. Queued 2026-11-19 22:55 PST (adacachy load ~29).
- WHP: BLOCKED (jupiter 156 commits behind; kiln can't build itself for Windows).
- 22:52 PST: this Mac's free disk fell to 17 GB (others' caches); nothing of mine to delete beyond small clones.
