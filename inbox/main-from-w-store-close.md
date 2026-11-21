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

## C3 CLOSED (2026-11-20): slates-store — the seam, landing, and the content-form A/B with the derived per-path arm
- Commits on origin/main (each confirmed with merge-base):
  - 6ce6bd170 the changeset log (crates/store/src/log.rs): CRC'd header, sections (kind, len, CRC-32C), trailer (BLAKE3
    identity, commit magic, len, CRC); appended in pieces, committed by trailer + one sync_data before the end moves;
    recovered to the first record that does not verify. On hyper-block directly (hyper-log's groups keep a truncated
    suffix; the history is never truncated) — recorded in A-767.
  - 6ada9dc42 the store: granted dir (land store_root) holding `changesets` + mantle's engine `rows`; landing = commit
    then C rows; engine checkpoints at the log's end; open replays the log from the checkpoint.
  - 67ba19d3a the seam: Staging (in flight by BLAKE3, moved not copied, bounded, Full past budget), Store::land from
    bytes or staged contents, P rows (payload identity -> place in the log, rebuilt on replay), Store::piece
    (borrowed in flight, read from the log once landed), Store::changesets/sections.
  - 6370667fc the A/B's arm C (crates/hg/examples/changeset_ab.rs): changesets + derived per-file form. Over hg's
    history at zstd 1, N=3, Adas-Mac-Studio-2: old read p50 73-74 µs, clone file section 0.06 s (bar: 91 µs, 0.27 s;
    A 77-79 µs / 0.19 s; B 814-833 µs / 2.02 s); derivation 2.2 s once, +25.7 MiB. Decided in A-767: changeset log =
    truth, derived per-file form serves reads. BENCHMARKS has every round.
- Tests: slates-store unit 7/7 (log over hyper-block's power-loss SimFile: round trip, every crash mode, interrupted
  record, cut at every byte, bit flips, out-of-order; staging), integration 4/4 on real dirs (rows by repository,
  recovery without/with checkpoint, a 300,001-byte pack read the same staged/landed/recovered), land store_root 3/3.
- Not met as asked: adacachy could not clone hg's history (mercurial-scm.org "incomplete response" x3), so the A/B rows
  are this Mac's (load 2-3.5). A Linux row needs the dataset carried there another way.
- Agreement on the stored form: w-hg agreed (named-base edits, Texts section — both in the format), w-git agreed
  (v4 payload by identity, normalization behind the same names); w-svn has not replied.
- Next: C4-C6 — the store into the server (StoreGrant at repository creation, the store on the owner shard), each
  tool's intake landing through Store::land, replay from Store::changesets, an RSS red test per tool; the derived
  per-file form built in the store with hg (C4).
