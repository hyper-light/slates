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

## 2026-11-20 ~21:30: memtable at both extremes, the sliced landing (71db6d157)
- Your memtable order: the derivation fix was already on main as bc0677af9. The memtable is one trunk node of extents,
  fanout × chunk = 512 KiB, with no machine input and no clamp. 71db6d157 adds the red/green test at both extremes:
  `slates-api` `store_sizing_tests::the_store_lands_on_a_small_and_a_large_machine_alike`. It injects 8 GiB and
  128 GiB into `facts.memory`, then runs `DaemonConfig::derive` → `start_store` → land → stop. Red at 128 GiB under
  the old formula (landing `Gone`), green at both. Ran on this Mac (Adas-Mac-Studio-2), debug, 0.75 s.
- On point 2 (the device's flush rate): the memtable holds rows only. A landing's bytes go to the log, the engine's
  write-ahead log, so the landing batch never sits in the memtable. The flush rate over the group-commit window is the
  input for the log's group commit, which is owed (GAPS A-767), not for the memtable. I didn't add a device input there.
- The section read for w-svn has been on main since ba760f0ef (`StoreHandle::sections` plus `read`), and w-svn was told.
- New in 71db6d157: the owner's sliced landing (`begin`/`section`/`part`/`commit`, abandoned on drop). Tests:
  slates-store 20/20 and slates-api store_sizing 1/1. Told w-hg, w-perf and w-svn.
- Next: C4 with w-hg, rerunning its store test on main; then C5 with w-git; then C6 with w-svn's import over the
  sliced landing.

## 2026-11-20 ~22:30: C5's store side; the grant's G row
- ad1d9e7a6: `StorePacks` (crates/server/src/vcs_store/store_packs.rs), git's sync `PackSource` over the store's
  owner. An awaited `fill` puts the covering 64 KiB store pieces into an LRU bounded by the caller's derived `most`;
  `piece` lends from that alone, and a miss is the cue to fill. There is no blocking read (R6). Test: a 300,000-byte
  pack read forward through a three-piece bound, each piece fetched once and every miss one fill (server --lib 1/1).
  Shape posted to w-git.
- e5baf401c: the standing grant is durable as the store's `G` row: recorded at the first open, checkpointed before
  anything lands, and found as first issued after a crash. Every landing's header names its id
  (`StoreHandle::grant()`); the creation landing does now, and the tools were told. Tests: slates-store 21/21, server
  store_tests 2/2. Still owed: a standing-grant `AuditKind` with its audit position, and revocation.

## 2026-11-20 ~23:30: a creation in the one landing order (e4cfbd38d)
- Red first: `a_kill_between_a_creations_landing_and_its_record_leaves_no_repository_and_its_retry_creates_it_once`
  (server --lib, `repository/store_tests.rs`). A test-only fault ends the landing task between its two steps. Before
  the fix, the repository was left in the register with no creation in the store.
- Fix: `land_then_record`. It judges the `RepositoryCreated` guard (no apply) and reserves the repository's and root's
  ids, then lands changeset 0. Only after that does one transaction record the repository, its root and the
  completion, and the client is answered (the verb's acceptance is deferred until then). A kill in between leaves a
  changeset no repository names, and the retry creates the repository once. Tests: `store_tests` 3/3.
- **Your call:** id reservation was needed because ids were only peeked from the partition's family counter when the
  verb ran. `ShardState::family_reserved_through` is a high-water mark; `fresh_family_id_at` now mints past it, so an
  id that never got recorded becomes a gap and is never reused.
- **Your call:** with a store, a create with no reply route (forwarded from another node) is refused `Unsupported`,
  since it has nowhere to be answered after the landing. This is in GAPS as owed with the fleet half. Overrule if you
  want something else.
- There was no GAPS line for this finding, so I recorded it as CLOSED in the A-767 entry, with a design status line.
- `repository::tests`: 58 pass and 4 fail. The 4 failures are the same ones (submit refused `Overloaded`) I reported on
  clean main f9c113ae9, not from this change.
- Sibling: svn_made's order is with w-svn (your note). w-hg's push already lands first on its branch.
