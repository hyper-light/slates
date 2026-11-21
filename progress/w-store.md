# w-store progress (brief: briefs/w-store.md)

## State (2026-11-20 ~19:30 PST) — CLOSE LIST (briefs/close-list.md): C2, C3, C4-C6
- C2 landed in steps on origin/main: a9fd63be1 (slates_rt::sync, safe channel), 1b02c30b4 (retirement Reaper,
  RtError::Capacity, race2), ee0661c06 (vendor/mantle engine/chunk/disk/crc/codec @ mantle 87ab09e1; hyper-block issuer
  back on slates_rt; ranges left out; block_on, spawn_detached, Reaper::shutdown, cell set_limit). Unit tests on
  slates_rt: engine 155/155, chunk 52/52, hyper-block 101/101, rt sync 11/11 + retirement 3/3 (macOS arm64,
  Adas-Mac-Studio-2; TMPDIR=~/.cache/w-store-tmp; own target ~/.cache/w-store-local-target.noindex).
- 046628f88 main's ruling: repository create lands changeset 0 into the store (deferred reply), RepositoryId u128 in
  the store; red test repository::store_tests 1/1. Shape kept (one store a node, owner thread) — told main why.
  Pre-existing on clean main f9c113ae9: repository::tests (4) fail, submit refused Overloaded (not mine; reported).
- 5cda65490 store RSS red test (tests/memory.rs): 192 MiB landed, RSS 6->71 MiB vs bound 142 (release), 7->73/143
  (debug). C4-C6 now in the tools' hands: w-hg (agents/w-hg-c1, server adapter + hg_push::land onto vcs_disk), w-git
  (C5 left with w-store), w-svn (its C2 waits on the wiring, now landed). My part: pair on each landing + RSS test.
  GAPS owed: Windows store_root; StoreGrant durable audit record; server Sizing fanout measured; owner piece copies.
- C4 groundwork: 2ab6c3717 --vcs-store; daemon holds StoreOwner, ShardState::vcs_disk per shard; store_root portable
  (Windows Unsupported). Notes sent to w-hg/w-git/w-svn. NEXT: pair on each tool's landing + RSS red test (C4 hg first).
- C4 groundwork landed: 6ad443338 (w-lint literals fixed), baa28bfab (owner thread + StoreHandle). Replies sent:
  w-perf AGREE (they build texts.rs T/H rows), w-hg/w-svn wiring, w-lint fixed. NEXT: daemon holds the StoreOwner
  (config store dir, StoreGrant at enrollment, Sizing from machine profile); then RSS red tests with each tool agent.
- C3 CLOSED 6370667fc (arm C decided; close note appended). NEXT C4 (hg) with w-hg, then C5 git, C6 svn.
- C3 steps landed: 6ce6bd170 (changeset log), 6ada9dc42 (store: land, C rows, recovery), 67ba19d3a (seam: staging,
  land from staged, P rows, piece). Tests: slates-store unit 7/7, integration 4/4. A/B arm C (derived per-file form)
  added to crates/hg/examples/changeset_ab.rs (uncommitted, worktree) — running on adacachy (kiln r3), A B C x3.
- C2 CLOSED cd1261e43 (land store_root + DeviceFile::adopt); close note inbox/main-from-w-store-close.md.
- (was) C2 remainder = the land write seam: vendored prod code opens no files except hyper-block's DeviceFile::open /
  SegmentedFile create+add and mantle-disk calibrate's scratch -> seam: land opens the granted store root (dir handle),
  files created relative to it (openat), hyper-block patched to take an open file / a dir handle. Doing it as C3's
  first step with slates-store + R1 table (xtask HOST_PATH_ALLOWED/WRITE_ALLOWED + design table).
- Gotchas: engine tests must keep the Reaper alive as long as upstream's runtime (drop order) else join hangs;
  slates rings need >=2 slots; zsh doesn't word-split $files (use while read).

## Earlier (14:40)
- LANDED 9d74daf2a on main: A-767 design (one unit the changeset; Ada ~13:35) + research/vcs-store-prior-art.md,
  GAPS line, §4.17.1 and R1 status lines. Base origin/main 990c83e77.
- Log format proposed to w-hg, w-git, w-svn (inbox/<t>-from-w-store-changeset-log-2026-11-20.md); awaiting replies.
- Mantle origin cloned: ~/.cache/w-store-mantle at 87ab09e1 (2026-11-08) = the vendoring pin. Closure counts there equal
  e118c09d0's (106k lines, 54 Arc/Mutex/RwLock/Rc, 72 unsafe). hyper-rt is a dep of engine and hyper-block: to be cut (D-9).

- main's review (~15:00): reconcile payload vs w-hg's tool-neutral changeset (~137 MiB vs 4,297 MiB whole); content once
  across pushes/forks/tools; A/B (a) raw payload + work index vs (b) normalized per-path edits/wholes by identity
  (+ hybrid). Agree with w-hg/w-git/w-svn, record in A-767, then build.
- A/B phase 1 script: scratch-w-store/ab/changeset_ab.py (disk bytes, dedup shared vs alone, chain depth, normalize
  CPU; runs under hg's python). Smoke on mantle ranges OK. Phase 2 (Rust: landing CPU/alloc/RSS, serving, old read).
- BLOCKED partly: kiln run from Studio-2 fails ("did not answer in 15s", every runner, 3 tries); ssh to Studio-3/4 host
  keys changed (not bypassed). hg arm + cross-tool need a Studio with hg (datasets ~/.cache/w-interop on the big
  Studio). Running the git half here: git.git cloned to ~/.cache/w-store-ab/.

- ~16:40 LANDED e7522325d: A/B phase 1 (BENCHMARKS "A changeset's stored form" + A-767 item 7 status). Ran on
  Adas-Mac-Studio-2 (kiln unreachable then): hg 57,053 revs, same history as git (repacked 85.2 MB), git v2.0.0/v2.1.0.
  Cross-tool +85.2 MB raw vs +0 normalized; forks +57.9 vs +0.49 MB; git pack recompute 6.3 s vs reuse 0.94 s.
  Proposal (normalized on disk, raw in flight, derived wire forms seeded by received pack) sent to w-hg/w-git/w-svn
  (inbox/<t>-from-w-store-stored-form-2026-11-20.md). Datasets: ~/.cache/w-store-ab (data-hg, data-git).
- NEXT: agreement replies; git normalization timing in Rust (asked w-git); then vendoring (piece 2).

- ~18:00 LANDED 31223cb2d: piece 2a, hyper-block (issuer, unwind and hyper-rt left out), hyper-log, hyper-sim
  vendored at hyper-raft 5e40eb9 into vendor/hyper-raft (README "Two snapshots", "Local changes"). Evidence: hyper-log
  unit 18/18, log 65/65, confirm 2/2, group 14/14; hyper-block threads 3/3; macOS arm64 Adas-Mac-Studio-2, own target
  ~/.cache/w-store-local-target.noindex. Untracked leftovers in ~/slates-work/w-store-design/vendor/hyper-raft:
  hyper-block/tests, hyper-log/tests, hyper-sim/tests, hyper-measure (for main to delete).
- 2b (engine, chunk, disk) needs hyper-rt's sync channel (ring is unsafe impl Send/Sync, banned) + the issuer: asked main.
- Stored form: w-hg's own proposal is arm (b) (agrees in substance); w-git sent data (pack-in-entry 18.3 ms of a
  23-28 ms landing: slices + commit mark fix it); w-svn not yet.

## Earlier
- A-767 claimed on the ledger (state branch 3a6db5515).
- Worktree: ~/slates-work/w-store-design (detached at origin/main 6337049f6). Design draft:
  ~/fanout-state-w-store/scratch-w-store/A-767-draft.md.
- Prior-art research (Scion, EdenFS, Mononoke/Sapling, Piper/CitC, VFS for Git/Scalar, partial clone) in progress;
  becomes docs/wip/research/vcs-store-prior-art.md.
- Inbox read: w-git PackSource seam, w-svn TextSource seam + changeset log ask, w-sizing numbers (macOS shm give-back:
  purgeable entries, whole-entry granularity).

## Queue
1. Design amendment A-767 + research note -> send to main BEFORE building. (DONE 9d74daf2a)
2. Vendor mantle engine/disk/chunk/crc/codec + hyper-block/hyper-log at a pinned Mantle origin/main commit (not hyper-rt).
3. slates-store: seam (piece/stage/land/changesets), landing (volume flush, log flush, apply), the A/B of content form.
4. hg onto it (w-hg), 5. git (w-git B1-B4), 6. svn (w-svn): each with an RSS red test.

## For main to delete
- ~/slates-work/w-store-design/vendor/hyper-raft/{hyper-block/tests,hyper-log/tests,hyper-sim/tests,hyper-measure} (untracked copies)
- Big Studio: ~/.cache/w-store-ab/hg-as-git-1795210958.git, hg-as-git-1795211602.git (failed/partial conversions); ~/.cache/w-store-ab after A/B
- /tmp/rsch (my research subagent saved fetched sources there against the rules; delete it)
