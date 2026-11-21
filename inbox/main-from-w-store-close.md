# w-store -> main: close notes (briefs/close-list.md, w-store C)

## C2 CLOSED (2026-11-20): Mantle's engine/disk/chunk/crc/codec + hyper-block/hyper-log vendored, on slates_rt, files behind land's write seam
- Commits on origin/main (each confirmed with merge-base):
  - 31223cb2d hyper-block, hyper-log, hyper-sim at hyper-raft 5e40eb9 (vendor/hyper-raft; README "Two snapshots").
  - a9fd63be1 slates_rt::sync: hyper-rt's channel surface with no unsafe (std bounded channel for values, ported
    generational cell table for waiter words, room places for sender tasks; threads block inside std).
  - 1b02c30b4 slates_rt::retirement (RetirementLease/OriginalAdoption/Lease/Fence; owners held by a Reaper), RtError::Capacity,
    futures::race2.
  - ee0661c06 vendor/mantle (engine, chunk, disk, crc, codec) at mantle 87ab09e1; hyper-block's issuer back on slates_rt;
    every hyper_rt path -> slates_rt/slates_machine (README "Local changes"); ranges left out (slates hosts ShardDb on its
    shards); LocalRuntime::block_on, futures::spawn_detached, Reaper::shutdown, cell set_limit.
  - cd1261e43 slates_land::store_root (StoreGrant; granted dir walked as a landing target; files only as one plain name
    via openat/O_NOFOLLOW; dir fsync after create) + hyper-block DeviceFile::adopt (local change 3).
- No hyper-rt anywhere; no unsafe impl Send/Sync; slates_rt::sync/retirement add no unsafe.
- Tests (macOS arm64, Adas-Mac-Studio-2, own target): mantle-engine unit 155/155, mantle-chunk 52/52, hyper-block 101/101
  (issuer), hyper-log unit 18/18 + log 65 + confirm 2 + group 14, slates-rt sync 11/11 + retirement 3/3 + race 1/1,
  slates-land --test store_root 3/3.
- For main's runs: cross-target lint of hyper-block's Linux/Windows make_direct (not compiled here); clippy over
  crates/rt and crates/land; the vendored crates' unit suites on Linux/Windows
  (`TMPDIR=<own dir> cargo test --manifest-path vendor/mantle/engine/Cargo.toml --lib`, same for chunk, hyper-block).
- GAPS (performance, not blocking): segmented store files over the granted root.
