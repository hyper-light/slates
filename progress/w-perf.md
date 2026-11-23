# w-perf (UI goal 93 part "An order of magnitude faster than competing servers, measured and recorded")

## CLOSE LIST (main 16:20, Ada's order): C1 goal 93 shared; R2 the hg import's peak -- status 2026-11-20 ~17:30 PST
- C1 CLOSED whole (inbox/main-from-w-perf-close.md):
  - svn storage A/B vs FSFS: a4b6a9a22 + 756104912 (records' deltas 33.2 MiB raw vs stock FSFS revs 41.0 MiB).
  - c3: 2502607c4 / 1df3d4700 (+ b51f908f2 WindowCache; replay 328 s -> 1.82 s).
  - deltify job: 1b168038c (merged e1d896547), form (A): `vcs_store::svn_deltify`, `Record::Deltified`, replay holds
    stored texts after the whole log. Test red (0,0,0) -> green (1,3,0).
- R2 (hg import 5.73 GB, through w-store's payload as ruled 15:40): IN PROGRESS.
  - b568499fe: revlog chains apply within two buffers (w-hg's question; red 263,219 -> 30,312 bytes charged).
  - Attribution: r16/r20/r22 invalid (wrong pid; MallocStackLogging and `heap` stall the daemon past its 1 s liveness
    window, the anchor kills it). r26: ps-only series + vmmap/footprint at done and landed only.
  - Dependency: w-store's slates-store (piece 3) not built (2b waits on main: hyper-rt sync channel); hg onto it is
    w-hg's C item. The cut through the payload waits on both; any non-whole-text share is cut now, red first.
- Main's 18:30 order, the derived per-path read form: DONE. fa5fd0023 (codec, T/H rows, reads, owner requests),
  c5d746f8b (first-landing T rows; reverted-text loop), 33465d641 (BENCHMARKS: p50 85-87 µs vs bar 91, 0/6,000 outside
  chain), 4f57efa69 (GAPS). w-store reviewed: keep as landed; closed begin/applied rows in bc0677af9.
- R2 now: w-store's memtable fix 71db6d157 unblocks w-hg. Waiting on w-hg's C1 to move the IMPORT onto the store
  (agents/w-hg-c1 has the push); asked w-hg 21:00. Then: measure import peak RSS vs .hg 87.8 MB (release, big Studio,
  ~/.cache/w-perf/bench/hgmem.sh, ps-only + footprint at landed).
- New findings (GAPS only, not worked): A-722 p3's partial clone 1.4-1.5x slower (sent to w-git; now on w-git's P list);
  same per-link apply loop in hg index.rs ~710 and import/record.rs ~1629 (told w-hg).
Brief: ~/.local/state/slates-fanout/briefs/w-perf.md. Supervisor: SendMessage to "slates-7a".

## Piece 1: A-699 harness + STOCK baselines (git daemon on git/git; then hg serve, svnserve) -- started 2026-11-19
- Predecessor g309's harness: branch agents/g309-scale (a303f3474, crates/cli/bench/git_scale.sh, wip x2, pushed).
  Studio scratch from g309: ~/.cache/g309-scale (src.git = git/git bare at f7de81456940), ~/.cache/g309-target.
- g309's numbers (r526, RUNS=1): stock git daemon push 30.3 s, clone 13.5 s (fsck clean, 1027 refs, 424,851 objects).
- LANDED 2026-11-20 ~04:55Z: main 3691deb5c..9f0851ea0 (piece + merge). Studio r7 (baselines, 04:21-04:47Z), svn tree
  check verified on a 20-commit dump (4,872/4,872). Studio scratch ~/.cache/w-perf: hg-src (Mercurial repo 57035),
  data/git-5000.dump (992 MiB), bench/, clone/ (shallow; read scripts via `git show FETCH_HEAD:...`).
## Piece 2: slates' numbers against those baselines (next)
- Main's rule: own target ~/.cache/w-perf-target (never ~/.cache/slates-local-target); release binary built from the
  named commit; keep >= 20 GB free.
- 2026-11-20 ~04:55Z: release slates built on the Studio from main 9f0851ea0 (A-704 in):
  ~/.cache/w-perf-local-target/release/slates (3m11s, CARGO_BUILD_JOBS=8); its source ~/.cache/w-perf/src-main (depth 1).
- git stock+slates (r13): stock run 1 push 17.8 clone 12.2 (checks identical); slates whole push FAILED: shard 10 spun
  1046 ms without a step, anchor killed the daemon, git "unable to rewind rpc post data" then hung (killed). Evidence
  ~/.cache/w-perf/evidence/. Reported to main; w-git owns the fix (main ~05:05Z), will tell me when it lands.
- Branch agents/w-perf-p2 (worktree ~/slates-work/w-perf-p2), commit "wip: hg_scale.sh measures slates' Mercurial site"
  (SERVERS/SLATES env; start_slates = anchor --vcs-http, repository create --tool hg, token as Basic password via auth.*).
- r19 (hg, SERVERS=slates, RUNS=1, log ~/.cache/w-perf/logs/hg-slates.log, work ~/.cache/w-perf/work-hg2) started
  ~05:00Z; kiln dropped (cert rotation ~05:09Z) before I read it. NEXT: read that log; if it worked, run
  SERVERS="stock slates" RUNS=3 and record; svn's slates side waits on the svn:// listener's repository lookup (A-689).
  Then git once w-git's spin fix lands. Then piece 3 (profile -> ranked plan to main).
- ~05:30Z hg by use: whole push 413; chunked 2000 and 250 also 413; import refused PastRoom{room:278528}. Reported
  to main (w-hg owns). A-710 (`slates repository import NAME PATH`) LANDED main 5c6eb940d with hg_scale.sh's slates
  side (SERVERS, SLATES, PUSH_CHUNK, SLATES_FILL=import). Test: Studio `cargo test -p slates-cli --bin slates
  repository` 3/3, fmt clean. Studio release binary now at 543606da3 (~/.cache/w-perf-local-target).
- NEXT: svn slates side (A-695 lookup + CRAM sign-in landed): svn_scale.sh SERVERS/SLATES, fill by
  `slates repository import project git-5000.dump` (svn), then svn:// checkout/log/commit/update vs svnserve.
- ~06:20Z LANDED main a11712269: svn_scale.sh slates side + KEEP_LOGS + svn rows in BENCHMARKS.md (1,000 revisions,
  main bc2803d66: import 1.874 vs svnadmin load 62.817; log 0.062 vs 0.140; checkout/commit/update even) and the
  blockers list. Reruns sent: svn 5000 at 9ea6e1311 still capped at 1,685 (w-svn = slates-work-3d); hg A-711 at
  bc2803d66: 413 gone, shard 10 spins 1,152 ms, daemon killed (w-hg = slates-work-09). Studio binary = bc2803d66.
- WAITING on: w-git push-spin fix, w-hg unbundle slicing + import PastRoom, w-svn green_chains cap. On each landing:
  rebuild release on the Studio (~/.cache/w-perf/src-main, target ~/.cache/w-perf-local-target), rerun that tool's
  harness SERVERS="stock slates" RUNS=3 KEEP_LOGS=..., record rows.
- Meanwhile piece 3 (profile slowest working paths -> ranked plan to main): svn checkout/commit (client-bound?),
  import memory (2.5 GB for 1,000 revisions).
## Piece 3: profile -> ranked plan (started ~06:30Z 2026-11-20), Studio, release bc2803d66
- svn checkout (1,000 revs): wall ~4.2 s both; client user 0.48 sys 2.78 (client-bound); server CPU slates 0.12 s vs
  svnserve 0.56 s. log: server CPU ~0 both.
- Footprint after import: 250 revs (85 MiB dump) ~920 MB; 500 (144 MiB) ~2,060 MB; 1,000 (223 MiB) ~2,430 MB
  (MALLOC_LARGE/SMALL ~2/3, VM_ALLOCATE (content pool) the rest). `status --json` has no memory counters.
- malloc_history (MallocStackLogging=1 on the anchor; daemon inherits), 250 revs, live heap 577 MiB:
  Importer::import 118.7; Repositories::apply_advanced 86.5 (259 allocs, ~334 KB/revision); merge history
  Value::whole 51.9; Partition::apply_green_checkpoint 46.8 (2 allocs); Green::commit 43.9; UnitSet::whole 42.7
  (segment, fixed); apply_repository 39.7 (257); merge engine apply 31.5+20.5; thread stacks 47 (fixed).
  Virtual only: timer Wheel::new 898 MiB (39 MiB a shard), segment/content-pool maps.
- Plan sent to main ~06:45Z; awaiting its pick.
- Slope 250->1000 (mh-250/mh-1000 on Studio ~/.cache/w-perf/prof): all linear. M2 corrected: the "Importer" heap is
  svn records' Repository.contents (texts whole), and the green's content pool holds the same files: texts held
  twice. Reported to main + w-svn (slates-work-3d); awaiting main's ruling on M2 (w-svn's code).
- LANDED main 183dd0e23: BENCHMARKS.md "Where the svn server's time and memory go, ranked" (T2 + M1-M5).
- Worktree now ~/slates-work/w-perf-plan (agents/w-perf-plan). Waiting on: w-git push spin, w-hg sliced intake +
  import PastRoom, w-svn green_chains; main's M2 ruling. On each: rebuild on Studio, rerun harness, record.
## A-715 (claimed; main's ruling ~07:00Z): svn texts held once (A-64), after w-svn's charging fix (A-714)
- Readers of Repository::content: records.rs (properties, Contents::mergeinfo, ReadSource::text/properties,
  drive::Green::text), Transaction::content/store/take_fresh_contents (importer delta base + dedupe), export.rs:542,
  svn_commit.rs text_bytes (fresh or records). Engine: Green::content_ref_at(path, version) -> Cow (version N = rev N).
- Seam proposed to w-svn (slates-work-3d) ~07:10Z: contents keep property lists only; texts -> TextPlace{path,
  revision,len}; a lent TextSource (green content_ref_at); log entry's record drops text bytes (green version carries
  them); rebuild replays green then records. Asked: green history trimming? which files A-714 touches?
- Also queued (main): git read path vs git daemon (clone, blob:none, shallow; git/git cut v1.0.0, whole if it fits)
  once w-interop's A-713 clone rows pass (w-interop r22).
- LANDED fdef525e3 (on main, checked): git row at cut v1.0.0 (release 82d3538b0): clone 2.816 vs 0.300 (9.4x slower),
  blob:none 1.092 vs 0.180, push 0.665 vs 0.310; sample: 72% of clone CPU in slates_git::store::deltas::Search::step.
  T4 (reuse deltas) proposed to main; awaiting ruling (w-git's area). A-716 seam awaiting w-svn's reply.
- A-716 seam agreed with w-svn (~07:40Z) with a change: green history IS trimmed (checkpoint_green folds below the
  floor, f37a96515), so texts are keyed by ContentIdentity in a store surviving folding, not (path, revision). The
  record's text bytes leaving the log entry also shrinks apply_advanced's ~245 KB/rev (w-svn hands that to A-716).
  A-714 touches db repository.rs + repository/entry.rs, server repository/push.rs, svn_commit.rs, vcs_store.rs.
## A-718 (main's ruling ~07:35Z; A-717 was w-git's): git deltas computed once and kept; clone streams stored deltas
- Done when: clone --mirror, blob:none, shallow re-measured vs git daemon on the Studio (cut v1.0.0), memory before
  and after, recorded. Ask w-git: can its sliced intake (A-717) record each pushed object's delta as it arrives?
- A-718 built: branch agents/w-perf-plan (pushed as agents/w-perf-a718), land worktree ~/slates-work/w-perf-a718-land
  (agents/w-perf-a718-land, squashed + merged main). Local green before merge: server t_a718+t_a682 6/6, git 2/2, fmt.
  Re-running server test on merged tree (main changed vcs_store.rs, A-714). Studio A/B (cut v1.0.0): clone-again
  2.766 -> 0.912 s (stock 0.342), blob:none 1.076 -> 0.558, shallow 0.154 -> 0.118, first clone 2.791 -> 4.293,
  RSS end 384-388 -> 482-513 MiB. w-git agreed A-718 lands first.
- svn 5,000 import at b705a24c5 (A-714 in): 3,540 revisions then Unpublished (record profile 803 MB > 747 MB segment
  region); svn commit after it E160004 green not at youngest (w-svn bug). Sent main + w-svn.
- QUEUE (main/Ada, ~08:00Z): A-718 land -> A-716 (svn texts once) -> concurrent-push load test (slim protocol client
  replaying a recorded receive-pack, then hg unbundle, svn commit; 1/10/100/1,000/10,000+; per-push memory,
  completions, refusals, p50/p99, footprint; sizes derived; RAM only) -> register entry objects named by identity
  (every tool; seam with w-svn).
- A-718 LANDED main 156f4437d (checked on origin/main). Merged-tree rerun green (server 6/6, fmt). Main had reversed
  "A-718 first" after it landed; told main. w-git's store-once intake merges over it.
- NEXT: A-716 (svn texts held once, keyed by ContentIdentity surviving checkpoint folding; record's text bytes out of
  the entry). Then the concurrent-push load test. Then entry objects named by identity (every tool).
- A-716 slice 1a LANDED main 5efce021a: merge held.rs identity table (hold_path shares the file block; hold_bytes;
  held; release_held; set_held_room; AtRoom/Holders/NoFile/NotHeld). Tests merge --lib held_tests 3/3 local.
  Worktree ~/slates-work/w-perf-a716 (agents/w-perf-a716). Load test draft on agents/w-perf-pushload
  (crates/cli/bench/push_load.py), worktree ~/slates-work/w-perf-pushload.
- NEXT: slice 1b: a green op that sets a path from a held identity reusing its block (ops-doc OpKind? or a
  push-builder op): read crates/merge/src/ops_doc.rs + push.rs; no re-hash on VFS path. Then 1c: held table in the
  checkpoint (new format version, presence byte like copies). Then load test (+ svn 5000 re-run after A-721: region
  wall, commit lands, log names it -> w-svn + main). Then svn slices 2-3 (seam with w-svn). Then entry objects by id.
- LANDED 54fa10ec5: push_load.py + BENCHMARKS (N=1/10/100 all ok; ~250-300 MiB kept per 3.4 MB push; 1,000 would OOM;
  attribution put_object 69.1 / entry 63.5 / arena 63.7 MiB). Main: 1b/1c OFF (Ada: push = work, content once by
  identity, D-28/§4.11/§4.17.1). No more load on the shared box until the plan is approved.
- Convergence plan sent to main ~10:00Z: S1 entries hold no file bytes (me, now); S2 history latest value = file block,
  held.rs stays as identity index, hold_bytes goes after S3/S5 (me); S3 git intake blobs as work ops + index (w-git);
  S4 svn records by identity (w-svn + me); S5 hg (w-hg). Bound: per-connection window + admission refusal, TCP flow
  control as backpressure.
- NEXT: S1: read crates/server/src/vcs_store/record.rs (RecordWriter, record_of) + repository/push.rs + db entry.rs;
  git entry record: keep moves + non-blob objects; blobs named by id, bytes in store; replay path reads them.
- A-723 S1 LANDED main de50bf0f5 (claimed on ledger 9f52804): git record v2 = names + pack as sent (Session::take_pack;
  pack::of_whole for server commits); replay via unpack reader (StoreBases, ReplayRefused::Pack); v1 still read.
  Tests local: server vcs_store:: git_push 58/58 on landed tree; git receive:: 46/46. Amendment written (end state,
  push bound, S1-S5, hot/cold duplication, owed: bounded parts across entries; held objects left out).
- S4 split with w-svn: its slates-svn slice 1 (TextSource trait, records name texts) now; my server slice 2 after
  (TextSource over Green::held, svn_commit drops text bytes from record, rebuild replays green first, held table
  in checkpoint or re-held at rebuild).
- NEXT: S2 (merge engine: content history's latest value names the file's block instead of a chunk copy). Then
  S4 server slice. Then bounded parts across entries. push_load re-run after S3 (w-git A-722).
- Worktree: ~/slates-work/w-perf-a723 (agents/w-perf-a723). Disk this Mac ~44 GB free now.
- ~11:30Z: found the 747 MB wall = partition SNAPSHOT (db replay.rs snapshot -> anchor publish Snapshot slot), the
  sum of all retained records; "bounded parts across entries" would not help -> not built. Main ruling (option 2):
  repository bytes leave the snapshot, become content in the anchor's content regions, entries name identities.
- S2 handed to w-engine; my notes at progress/w-engine-notes.md (state branch 87a26ad).
- Design draft sent to main ~11:40Z: vcs ContentStore windows in the shard's chunk arena (A-64 anchor content object),
  its table in the shard recovery image; git record v3 = names + pack root identity; order: content in arena ->
  shard image published -> entry commits; recovery: images first, then records; replication content before record.
  Slices (a) store over arena + image, (b) record v3, (c) svn via S4, (d) replication order. Awaiting OK + A-number.
## A-727 (claimed, ledger 5f5f1d6; main OK ~11:50Z): repository bytes as content in the anchor arena
- Design (sent to main, approved): vcs ContentStore windows in the owner shard's chunk arena (A-64 anchor content
  object), its table (identity -> extent ref, form, holders, lists) in the shard recovery image, recovered BEFORE the
  partition's records; records name identities (git v3: names + pack root identity); GROUP COMMIT: entries wait on
  the next image barrier, one barrier covers every push written before it (main's requirement; measure at load).
  Replication: content before record.
- Slices: (a1) slates-vcs: storage seam in ContentStore (`Box<dyn WindowStore>`-like, heap default; no generic
  ripple into git/hg/svn which take &ContentStore). (a2) server: arena-backed window store over the shard's
  ChunkArena (state.store owns the arena; vcs_stores separate -> access by extent handle; mirror how vfs ChunkStore
  reads extent bytes); deferred-free rule (A-64: a block the committed image names is not reused before a newer
  image commits). (a3) image section: vcs content table in the shard image + recovery restores ContentStore entries
  before records replay. (b) git record v3. (c) svn via S4. (d) replication order test.
- Facts: ContentStore today = BTreeMap<ContentId, Held{bytes: Box<[u8]>, form, holders}> (crates/vcs/src/content.rs
  insert_new ~412); ChunkArena API crates/mem/src/arena.rs (alloc/free/give_back/lock_extent, Extent{..});
  partition snapshot publish crates/db/src/replay.rs snapshot (~673); anchor segment publish refuses past slot
  (crates/anchor/src/segment.rs ~1041).
- NEXT: read vfs ChunkStore (crates/vfs/src/content.rs / chunk store) for extent read/write + how the shard image
  (vfs recover.rs to_image / encode_image_into) lists blocks; then write the A-727 design amendment (docs) and land
  it; then (a1).
- A-727 design LANDED main 41f183ce6 (docs only). Build slices next: (a1) read seam in slates-vcs ContentStore::windows
  (9 production readers in git/hg/svn/server) -> copy-out read_into / Cow; heap storage behind a seam. Then (a2) arena
  storage via vfs ChunkStore (open/write_open/seal/read_extent_into/claim_chunk_blocks/adopt_chunk), (a3) image
  table + recovery before records, (b) git v3, (c) svn, (d) replication. Group commit for image barrier per push.
- A-727 (a1) in progress, worktree ~/slates-work/w-perf-a723 (agents/w-perf-a723): ContentStore::windows and
  window_at answer Cow<[u8]> (get stays &[u8]: lists/tables are the store's own metadata, kept in memory); readers
  adapted: git derived/tree.rs, store/upload.rs (868 &first, 917, 951 Cow::Borrowed(made_window), 1377, 1599),
  store/views.rs as_kept -> Option<Cow> (a borrowed window lent, an owned one kept like assembled); hg import/record.rs,
  push/held.rs, render.rs, serve/indexed.rs (copied generic over AsRef, framed chains Cow MARKER, lend counts borrow);
  server commits.rs, record.rs; vcs blame.rs; tests oracle.rs, hg tests/import.rs, vcs content tests. Compiles.
  Focused tests running (vcs content/derived, git store/derived/upload, hg serve/import/push/render, server vcs_store/git_push).
- A-727 a1 LANDED main f8c9c95cd (windows/window_at -> Cow; readers moved; fixed w-git's intake.rs on merge). Tests
  local on landed tree: server 63/63; earlier vcs 51, git 167, hg 111.
- Seam with w-balance (Studio-2, kiln id 01a1f7b42ec27702a195501719d3f919; reach by agents_send): agreed. Its
  HomeTicket + homes::image_begun(g)/image_published(g)/published(origin,ticket,budget) on main 93c0e64cc. Windows
  live on identity-home shards (homes.rs); owner image carries non-home rows {identity,len,home}. Batch forms after a3.
- By-use at f8c9c95cd: push_load N=1/10/100 = 205.9/165.9/40.2 MiB a push (was 300.8/289.9/248.8), all ok.
  hg import of Mercurial repo: crash loop (shard 9 spins >1 s at every restart, 11 restarts) -> w-hg + main.
- NEXT: (a2) arena storage on one shard (windows homed here) for slates-vcs ContentStore: storage seam (heap vs
  arena), arena backed by the shard's vfs ChunkStore (open/write_open/seal/read_extent_into); then (a3) image table
  + recovery + barrier hooks image_begun/image_published; then (b) git v3; S4 slice 2 (svn texts via WithTexts).
- ~13:00Z rulings: A-727 (a2) = option (i): repository store's OWN chunk arena per home shard behind a storage seam,
  extents from the anchor content pool under its own owner-tag range (per home; w-balance's holdings read from it),
  its own image section + recovery; no ripple into git/hg/svn. Rule (B): green = head files (hot); repository store
  = compressed history (packs as sent, revlog/svndiff deltas) + tool metadata, NEVER inflated file bytes; non-vacuity
  counter "blob put into the repository store" asserted 0 on a push once S3 lands. S4 slice 2 after (a3): when the
  head moves the old head's delta must be durable in the repo store before the green folds that version; test fold ->
  restart -> svn cat -r N.
- (a2) step 1 (slates-vcs): WindowStorage seam (heap impl default for tests/tools); windows go through it, lists/tables
  stay in the store's memory; get() lends lists/tables only (git derived/tree.rs:813 gitlink read -> windows()).
- (a2) step 1 built in worktree ~/slates-work/w-perf-a723 (uncommitted): slates-vcs WindowStorage trait (put/read/
  release), StoredWindow handle, HeapWindows default, ContentStore::with_storage, Held.kept = Kept::Here (lists, tables,
  derived) | Kept::Stored{window,len} (windows); get() lends Here only; new bytes_of() reads any form (Cow); release
  copies a stored window out then releases it; ContentRefused::Storage; git derived/tree.rs gitlink read -> bytes_of;
  vcs tests moved to bytes_of; new test windows_kept_by_a_storage_that_copies_out_read_and_free_as_the_heap_store_does.
  vcs content:: 8/8. Dependents' tests running. Then: land step 1; step 2 = server ArenaWindows over a repository
  ChunkStore per shard (pool owner-tag range), then (a3).
- (a2) step 1 LANDED main 6b1fb644f (WindowStorage seam, HeapWindows, with_storage, Kept Here/Stored, bytes_of,
  len_of; readers moved incl. homes read_here, kept_fragments). Landed tree: server 72/72, git store 87/87.
- hg import re-run (A-732, w-hg's fix) running on Studio as kiln r66 (log ~/.cache/w-perf/hgimp2.log/.out, evidence
  hg-import-2.log). Report result (completes/typed refusal, no spin/restart, RSS) to w-hg (slates-work-09) + main.
- NEXT (a2) step 2: server ArenaWindows: a repository ChunkStore per home shard (vfs ChunkStore over a ChunkArena whose
  ExtentSource = ContentPool with a distinct owner-tag range; content_pool.rs owner word = partition+1 today),
  installed via ContentStore::with_storage where VcsStores.content is made; reads via read_extent_into (copy-out).
  Then (a3): image section for the repository arena's chunk table + recovery before records + homes::image_begun/
  image_published hooks + group commit; then (b) git record v3; then S4 slice 2 under rule (B).
- (a2) step 2 PARKED on branch agents/w-perf-a727-a2s2 (08cb0ddd0): ContentPool::open_repository (owner
  partitions+partition), daemon repository_windows(), ArenaWindows (seal/read-copy/free; tests pass), VcsStores::
  with_windows wiring. FINDING: in a real shard the repository arena is starved: first put ArenaExhausted, grow adds 0
  (volume arena holds the slice's pool extents); hg_made t_a731 test fails with it, passes with heap storage. Also the
  shard budget tracks only the volume arena. Asked main to rule: (i') layout range per slice, (i'') budget-grown +
  balancing, or (ii) windows as chunks of the volume ChunkStore lent per step (recommended). Waiting.
- hg import re-run (A-732) r66 still running on Studio (or done: check logs hgimp2.out / evidence/hg-import-2.log).

## A-727 (a2)(ii) -- LANDED 2026-11-20 479f950ed (merge d962caeb7 + second merge; merge-base confirmed)
- Windows = chunks of state.store.content (ChunkWindows), lent per step by swap+guard (vcs_store::Lent, lent_state,
  take_chunks/give_chunks for lfs). Unmade storage lent too; arena grows from pool on demand, budget extended at
  hand-back; release outside a lend frees at next lend. Parked branch agents/w-perf-a727-a2s2 deleted (remote).
- Fixed slates-vcs 3 red tests from my f3429bdcd (get() Here-only) -> bytes_of/len_of.
- Merged tree: server lib 954/954 run (skip content_ranges a_sweep_completes: A-744 test spins a debug shard alone,
  reported to main), vcs 100, hg 356, git 557.
- PRELIMINARY (debug build, not a benchmark): swap pair lend_in+hand_back 153 ns; ChunkStore 560 bytes. Release figure
  owed on the big Studio (adas-mac-studio.local-2) via kiln.
- r66 (hg import) was a pre-A-740/pre-98bcb0002 build: NOT current. Main wants an interleaved A/B on the big Studio:
  b1d001601 vs 5ad8dea09-or-later, wall time, peak RSS, record bytes; A-740 spin line names the task; whether spin
  recurs after each restart or only on CLI retry -> w-hg (slates-work-09) and main.
## NEXT
1. vcs_store::tests::assert_made_again_whole holder mismatch after a plain git push (create, update main): live store
   has extra holders vs rebuilt. Failing test first; make assert_made_again_whole usable in git_push tests.
2. hg import A/B (above). 3. release swap cost on big Studio. 4. main: daemon_edge long-history fetch flaky (A-718
   encode in long turns) -> slice encode under step budget or encode at intake. 5. (a3), w-balance review, S4 (c).
- 78d5454a4 LANDED (holders made-again: live store compared settled; test t_a727_a_plain_push_then_its_update...).
- hg import A/B: kiln r72 building release A=b1d001601 (src-A, tgt-A) and B=main de381e643 (src-B, tgt-B) on Studio
  d1323898 (~/.cache/w-perf/bin/slates-{A,B}). Then ab.sh: A1 B1 A2 B2, wall, daemon peak RSS, status --json, spin
  lines (A-740 task), restarts, timestamps to correlate spin vs restart vs CLI retry. Release swap probe in src-B.
- w-balance record.rs v3 edit: not pushed yet (no branch touches record.rs).
- main: the B run of the hg A/B is also the S5 import-half "before" (records full texts in A=b1d001601; B has
  98bcb0002 delta+base). Record wall, peak RSS, record bytes per run, best of N with all N, in BENCHMARKS.md.
  Record bytes: bench-only eprintln "w-perf-bench: hg import record N bytes" patched into both src trees after
  writer.finish() in hg_import.rs (not committed). Driver: local ~/.cache/w-perf/studio/ab.sh -> Studio.
- w-git run request A-722 part 3 (82ff3bc79, in B): push_load.py / git_scale.sh rerun on bin B after the A/B.
- w-balance: agreed they write names_to_content/names_from_content; I call them in record v3 (A-727 b).
- Local: build with CARGO_INCREMENTAL=0; delete debug/incremental at each landing.
- r75 (Studio, bg): rebuild A/B with bench line, release swap probe (src-B, -> ~/.cache/w-perf/swap-probe-release.txt),
  then ab.sh 3 rounds CAP 1800 (out ~/.cache/w-perf/ab/<ts>/results.tsv + per-run .log/.out/.peak).
- main dropped the bisect (cause established, fixed 479f950ed). No compound rm over variable paths (kiln gate).
- w-balance: rows codec on main slates_vcs::rows (0011a4606); their record.rs v3 branch lands right after my a3.
- NOW (local, while r75 runs): A-727 a3 -- image table of ChunkWindows' chunks + recovery before records.
- A-727 a3 LANDED 782eb711c (windows as held pieces kind 11; restore before volumes; release_unheld after replay;
  test daemon::tests::t_a727_repository_windows_survive_a_warm_restart_in_the_shard_image). Told w-balance + main.
- Local target now ~/.cache/w-perf-local-target.noindex (old one wiped, 45 GB), CARGO_INCREMENTAL=0.
- r75: release swap pair 38.83 ns (10M rounds), ChunkStore 560 B. A/B round 1: A-1 cap 1800 s, RSS 23,969,952 KiB,
  record 10,049,541,313 B; B-1 record 4,657,833,574 B; both loop: record finished -> landing spins (import.rs:235
  task) -> restart -> CLI retry re-imports. Reported to main + w-hg. Rounds 2-3 running; then BENCHMARKS.md entry.
- NEXT: A-727 (b) record v3 is w-balance's branch (I review the call site); push_load/git_scale rerun on bin B for
  w-git's A-722 p3 after the A/B; main's daemon_edge long-history flake (A-718 encode in long turns).
- 8fac3f04f LANDED: fetch's encode wait bounded to LIVENESS_BUDGET_NS + encode_behind task (A-718 continued).
  Long-history push edge test still flaky (push answer; reported to main, not taken).
- w-balance: barrier driven by held_committed (3f4996b5f); v3 names each names window (count + 32 B/window),
  lists not imaged; v3 waits for main's f+1 ruling. I review their record.rs call site.
- w-hg cutting hg record (file deltas); will ping for rerun. hg A/B r75 rounds 2-3 running -> BENCHMARKS.md.
- NEXT after A/B: push_load/git_scale rerun on bin B (w-git A-722 p3), BENCHMARKS entry (A/B + swap 38.83 ns).
- r79 built C = de49b6bd4 (bin/slates-C, bench line in). r80 (bg): waits for r75's ab.sh, then VARIANTS=C ab2.sh 3
  (CAP 1800) -> new ~/.cache/w-perf/ab/<ts>/results.tsv. Then BENCHMARKS.md: A x3, B x3, C x3 + swap 38.83 ns.
- Waiting on main: S4 slice 2 cut (c1/c2/c3, skip A-716 1c by re-holding head paths at rebuild).
- Seam answer to w-engine pushed (state 1fc0f6ebd), accepted by main. w-svn staging API on main (e9b70d6d4).
- S4 c1 LANDED 17aa54dd8 (svn_texts: hold/release per landed revision, rehold_head at recovery, HeadTexts source).
  Reads switch to HeadTexts + drop records' text bytes + drop fallback = c3 (one series; GAT refactor of SvnStores).
- NEXT: c2 (old text as svndiff delta vs successor in repo ContentStore before the green folds); BENCHMARKS on r80.
- S4 c2 LANDED 14215b0c3 (skip-delta records). main ruling: sink texts land as Stored{len} refs (w-svn), my c2 encoder
  becomes a background sliced deltify job on supersede (hooks stored_texts()/supersede() from w-svn). Fix records.rs:231
  `mut fresh` warning in next landing.
- Identity rule (main): cross-node identity = plain BLAKE3 of bytes; store root node-local. Note pushed state 2db786af6.
  IN PROGRESS (uncommitted, w-perf-a723): ContentStore objects/rooted alias maps, put_object/finish_object return BLAKE3,
  resolve() in every lookup + release cascade, root_of/root_id for old records (git replay same_name/alike compat);
  vcs 104/104; git/hg/svn/server suites running (bze1r0hhg). NEXT a3b: image lists + aliases (main: must survive restart
  for v3), restart red.
- A/B/C hg import DONE -> BENCHMARKS entry written (uncommitted): A 10.05 GB rec/23.9 GB RSS, B 4.66/10.1, C 1.30/10.7,
  C refused by snapshot slot after a spin. Sent to w-hg. Main's svn A/B (whole vs skip-delta vs FSFS) still owed.
- LANDED: identity rule 9010c5630; a3b 9b9d8dec6 (landed tree bf10a773a verified 189/189). Signalled main (now
  slates-72? sent there; slates-7a gone) to mark goal-93 steps complete; part itself NOT complete.
- OPEN (mine): background deltify job (supersede hook from w-svn), svn storage A/B (whole/skip-delta/FSFS, all N),
  c3 (drop records fallback), review w-svn's sliced exporter `load`, push_load/git_scale rerun (w-git A-722 p3).
- w-lint red (content-once test) passes at bf10a773a; asked main for w-lint's base.

## 2026-11-21 ~22:00 PST (main 01a20182's note, inbox/w-perf-from-main-01a20182-ssh-rekey.md)
- Landed today: 70dd82623 (sha1_ab, paced intake slices 406fa7f35), 2ab402b95 (HTTP fetch keepalive), 1a8859a3f (SSH
  fetch keepalive).
- CLOSED by A-842 (w-svn cc55404fa): c3 (records keep no text; fallback gone) and the background deltify job. Under the
  changeset ruling, svn deltas are derived, never stored.
- e0d79f557: already landed in adapted form as 406fa7f35 (70dd82623); nothing more lands. Its checks_bytes part stays
  with the held fan-out 2abe7cae3. The other held P commits stay held until goal 93's servers are complete and correct.
- NOW: the SSH edge's rekey write duplication (goal 93 C). Waiting on w-hg's written go
  (inbox/w-hg-from-w-perf-ssh-rekey-go.md); reading meanwhile.
- ~22:45 PST: STOOD DOWN from the SSH rekey write duplication (main 01a20182; inbox/w-perf-from-main-01a20182-rekey-closed.md).
  w-hg landed the fix as a3e673efc (`TcpStream::write_some`, red `vcs_ssh::tests::hg::a_rekey_during_a_blocked_write_sends_no_byte_twice`).
  My uncommitted slates-rt `TcpStream::write` and its test were dropped unlanded; nothing of mine is in vcs_ssh.
  Back on goal 93's reserve, no runs.

## 2026-11-22 ~05:10 PST: 84a (macOS p999, then Linux), main 01a20182's piece; stopped at main's §8 correction
- LANDED: 4a1686023 (docs only). The refining job's paced turns, two arms, were measured and rejected: dead even with
  main on one runner's interleaved rounds. BENCHMARKS "84a on macOS: the refining job's turns paced" holds both runs'
  tables (45006705328, 45010946191); GAPS 84a has a line.
- macOS read, stamped under TB (diag-measurer-w-perf-84a, never main):
  - No wait holds p999. Shard parks: 0-1 of ~16k a round waited >= 1 ms past their timeout. Client parks 0.9-1.2 ms:
    3-10 of ~73k. The client's wake has a median of 4-12 µs in the tails.
  - The slow classes:
    - submits parked behind their refinement (the job's yields plus the resumed run, all CPU);
    - lease_wait work-queue verbs (2.6% of requests, no single leg).
  - Round-to-round variance dominates. The macOS close needs more rounds a run, main's call.
- Linux read (runs 45017505026, 45018893846):
  - Kernel 7.0 azure, HZ=1000, PREEMPT_LAZY, RUN_TO_PARITY, base slice 2.1 ms.
  - A woken or runnable shard doesn't get its CPU against the spinners: arm kick -> run p50 920 µs; x86 queued 2.8-3.9 ms.
  - RLIMIT_RTPRIO is 0:0 on the stock runner account.
- Linux arm, under an administrator's RTPRIO 1:1 grant (sudo prlimit, both arms; the daemon asks nothing, R10):
  - B 21179849c, the shards in SCHED_RR (grant or skip): kick -> run 921 -> 8 µs (stamped run 45020845000). The tail
    moved to the client's time-share wake (2.5-3.0 ms). Plain A/B 45020495221: no starvation; the quiet path worse
    (p50 +10-35%, arm R9 p99 82 -> 111 µs).
  - D 69c483e8d: B plus the client's reply-wait park in RR (sched_getattr, exact restore, a typed refusal if refused,
    counted).
  - C 0a7e06c56: D with the shards spinning R9's floor alone at RR, floor spins counted.
  - Four-arm A/B: run 45023871879 (branch diag-measurer-w-perf-84a-linux-ab, A d2c6635a2). Mine to read.
- To LAND C (or D) if main's table says so: a clean branch from origin/main, cherry-picking d2c6635a2, 21179849c,
  69c483e8d and 0a7e06c56 only (never the diag workflow commits on that branch). Then focused tests (slates-rt and
  slates-machine round_robin, slates-ipc endpoint), fmt; design, GAPS and BENCHMARKS lines; claim an A-number.
- Rule (main 05:06 PST, correcting its own 05:05): an arm's interleaved design A/B is the agent's own to run and read
  (Ada, 2026-11-20: "make your agents do A|B testing"). Still main's: clippy, other platforms' lint checks, suites, a
  landing's confirming rounds, and the wall.
- ~08:30 PST: 84a Linux CLOSED as measured-and-rejected. LANDED 519fe5568 (docs only): BENCHMARKS "84a on Linux: the
  round-robin real-time class...", with the stamped run, the kernel facts, RTPRIO 0:0, and the four-arm tables
  (45036903025), plus a GAPS 84a line.
  - None of B, C', D' meets 1 ms. C' is best on conflict p99 and holds quiet and R9, but its accept p99 is
    0.94-2.6 ms and the spinners lose 20-27% of their throughput.
  - The remaining tail is the client's own time-share code waiting whole 2.1 ms slices (structural). Open as main's
    call: a less oversubscribed lane, or the bar re-stated for Linux.
  - No code landed. The arms are on origin/diag-measurer-w-perf-84a-linux-ab and origin/agents/w-perf-84a-arm-d2.
  - Lesson recorded: SCHED_RESET_ON_FORK can't be cleared unprivileged, so a temporary raise must not set it.
- 2026-11-22 ~13:15 PST: 84a macOS p999 NAMED, the doorbell arm measured, NOT landed. LANDED 185217879 (docs only):
  BENCHMARKS "84a on macOS: the doorbell thread under the time-constraint policy, measured, not landed".
  - Timed-only join (45065854565): the p999 tail landed on parked shards whose parks ended early (late, not lost).
  - Hop stamps (45067781801): the client rings within 2-8 µs, the doorbell thread wakes 0.15-12 ms later (max 17 ms),
    and the kick and run take tens of µs. 0 lost wakes. Linux (45067930278) has no bell signature.
  - Arm 6aa639211 (G222_BELL_TB, diag): rung -> woke is 4-8 µs, but p999 is under 1 ms in only 1 of 8 rounds in
    both arms. The p999 is bound by serve_loop polls of 4-15 ms ("task (long)"). Quiet accept p99 was worse in 5 of 8.
    Retry once the polls are bounded.
  - Running: 45071270749, serve_round's parts stamped as long serves by name. Next piece (main): fix that
    bounded-work bug, red first (a poll stays within its quantum under that load). Then Acknowledge, CreateWork,
    Destroy. Then retry the bell arm and re-measure Linux's p999. Watch item: a client descheduled 13 ms between push
    and announcement read (once).
- 2026-11-22 ~16:30 PST: 84a, two threads open, nothing landed since 185217879.
  - macOS p999: the serve_loop holds are the reply write's wake_one, 1-33 ms of on-CPU kernel time
    (os_sync_wake_by_address_any, SHARED). Runs 45072617323, 45072811313, 45074388808.
    - spindump (45082342692): 69 of 106 samples sit at one kernel PC, a spin. It's unsymbolicated (needs the 25G332 KDK).
    - Read: the VM's lock-holder preemption on the shared ulock. Probes 45078071137 and 45079961886 don't reproduce it.
    - Race probe 45086631612 is running. Main runs the bare-metal check from fffa0046d (diag-measurer-w-perf-84a-mac-tail).
    - The §3 long verbs went to codex-review (main, 2026-11-22).
  - Linux arm F (Ada: "fix it"): agents/w-perf-84a-arm-f b44f0904f on E. The client pins to its shard's CPU for each
    synchronous request; the region gains the shard's CPU word (layout version 5).
    - F_diag 9ad9d4cc9. The A/B is 45087231374 on diag-measurer-w-perf-84a-f-ab.
  - Worktrees: w-perf-diag and w-perf-armf. w-perf-land was removed (clean) to make room; re-add it to land.
- 2026-11-22 ~18:30 PST: LANDED A-867 (76bbf6ca8, merge 156e5ea30): a macOS client's synchronous park waits on its region's
  Mach semaphore, its send right carried with the region's fileport through mach_claim's keep and fetch.
  - A/B 45091701553: reply wakes >= 1 ms 3,001 -> 36; loaded p99 median accept 1,766 -> 976 µs, conflict 4,301 -> 1,363 µs.
  - Bare metal (main's Studio check) has no slow wake: the address-wake spin is the CI VM's.
  - Red first: the cross-process test failed while the semaphore was named in the header (KERN_INVALID_NAME).
  - Focused tests on macOS: ipc lib 80/80, rendezvous 16/16, completion_memory, rings, client 39/39.
  - Main's: clippy (macOS, Linux, Windows cross-lint for slates-ipc), xtask unsafe (slates-ipc 160 -> 167), and the
    confirming rounds on the landed code.
  - Arm F rejected and recorded (6bab42576). Arm G handed to main (agents/w-perf-84a-arm-g 674c030f5).
  - Next mine: init_shard unit 3 (boot's 2-68 ms slice).
- 2026-11-22 ~22:30 PST: LANDED 80412e446 (merge 794619d6a): a shard's start holds each heavy piece of its setup in a slice of
  its own.
  - The setup after the catalog's replay was one unit: 12-21 ms on CI macOS, 59-68 ms on the Studio.
  - What changed: RecoverySlices::piece_done (eight named pieces) and StartReport via Daemon::shard_start_reports; an
    attach opens its segment once (SparseObject::declare, opens_on_this_thread); UnitSet::whole fills words whole.
  - Red to green: 8 -> 1 heavy pieces in one slice (heaviest 9.0 -> 1.9 ms, the pool's segment); 2 -> 1 opens an attach.
  - For main: the CI boot lines on the landed code; clippy and the cross-lint for server, anchor and mem.
  - Pre-existing on this Mac: anchor a_table_past_the_old_exec_limit_reaches_the_successor_whole fails at HEAD too.
  - A-867's confirming rounds (main, 45104222730): no p99 difference on a fast runner; it stays landed on the slow
    runner's evidence.
  - Nothing left in my queue.
- 2026-11-22 ~23:30 PST: LANDED b1654ef67 (merge 8883ebfd1). A send into a full macOS hold channel waits for room within the
  caller's liveness budget; before, it failed ENOBUFS.
  - a_table_past_the_old_exec_limit (13 of 7,792 held) is green, anchor 33/33.
  - Added held unit tests 44/44.
  - Bug record docs/bugs/2026-11-22-a-full-macos-hold-channel-refused-holds-the-anchor-would-have-taken.md.
  - Siblings: none (every other pair is a stream or Linux seqpacket).
  - For main: clippy and the cross-lint for slates-anchor and slates-server, a Linux run of anchor and held.
  - Queue empty.
- 2026-11-23 ~00:30 PST:
  - ead8aee9a: the bug note says the full-table test never passed on macOS (A-462 ran lib tests only, no CI reached it).
  - G+E's shard regression named for main: naps keep the shard's EEVDF negative lag, because DELAY_ZERO clips only
    positive lag (kernel v7.0 fair.c).
  - LANDED 9ff6e401a (merge e4fb444ab), the nightly 45017186325's gates in my crates: heap::system reads no allocator
    under Miri; TSan skips the heap test; wire_order publishes ResolveConflict (101) and DestroyRepository (102).
  - Routed: client lone_daemon (a timing bound under TSan) and merge shuttle_green (Outcome::ContentUnread, A-760 p1).
  - Queue empty.
- 2026-11-23 ~01:30 PST: LANDED ea3240fd8: client lone_daemon's outstanding call is bounded by the driver's own look after
  the exit (A-523's doubling), not a reply deadline.
  - The bound predated A-523 by a day.
  - Red without TSan here too (27.19 s vs 24.16 s); now 4/4.
  - Design note to main: the async driver could watch Linux's departure socket.
  - The nightly-gates commit is 9ff6e401a (I first sent main b57a2dbaf by mistake).
  - Queue empty.
- 2026-11-23 ~03:00 PST:
  - Arm H's A/B read (run 45108777338). H loses loaded on both legs and is even quiet. My prediction was wrong: the
    client half alone carries most of G+E's regression.
  - LANDED 78c5c5b1f (merge c760bfc1e): BENCHMARKS "84a on Linux: naps on a thread's own timer" (G+E and H rejected,
    runs 45104143760 and 45108777338, main's arms) with the rtkit probe (run 45115263387: no RealtimeKit on either
    runner); one GAPS line.
  - LANDED 75a8e7df9 + 3d61fd9b3 (merge a744fc668): a request's ring.request span opens at its own slot read and ends
    at its reply's publication, before the wake (DaemonEnd::reply returns the reading; send_reply returns Sent).
    - Red first: a_requests_ring_span_lies_within_its_clients_own_time (server/tests/daemon.rs) failed on the Mac, a
      span 165 us before its request's publication; green after, and again after main's merge.
    - Bug record docs/bugs/2026-11-22-a-ring-span-outlasted-its-clients-wait.md.
  - For main: clippy and the cross-lint for slates-ipc and slates-server (DaemonEnd::reply now returns u64). The
    attribution's ring/outside split is good only for runs built after a744fc668.
  - Queue empty.
- 2026-11-23 ~05:00 PST: LANDED 0227ad029 (merge 0bc10f567), goal 93: the watchdog's beat allocates nothing.
  - LatenessTail is a fixed ring of A-474's hundred readings, its p99 a sorted stack copy; the beat refills its looks,
    windows and verdicts in place.
  - Red first: the_watchdogs_beat_allocates_nothing_once_it_runs (server/tests/watchdog_allocations.rs, a counting
    allocator on the thread named slates-watchdog): 82 idle and 81 under 71,073 requests over 20 beats on HEAD; now 0
    and 0, again after main's merge.
  - The ring's oracle against the old queue (watchdog lib tests 30/30). GAPS line under goal 93's memory entries.
  - My cargo fmt also dropped a stray blank line in vcs_store.rs that main carried.
  - For main: clippy for slates-server and slates-api (anchor.rs uses LatenessTail); hg_push_heap before and after.
  - Queue empty.
- 2026-11-23 ~07:00 PST:
  - LANDED 27ee14b45 (merge ec7522621): the watchdog's allocation figures as measured (4 a beat, ~1 KB by types,
    now 0); the 24% / 10-15 MB was hg_push_heap's sampler artifact (f14ea81bb). Main kept 0227ad029.
  - LANDED d2c7a6ca5: Windows long polls attributed.
    - slates_machine::own_cpu: QueryThreadCycleTime at a rate measured once a process (best of 8 spans of 1000 clock
      steps; 3.14 GHz on jupiter), warmed in Runtime::start_with_sized.
    - switches::voluntary_waits_now: every profiling read folds PERFORMANCE_DATA's WaitReasonBitMap into a count of
      reads that saw a wait other than WrQuantumEnd/DispatchInt/Preempted/YieldExecution/DeferredPreempt.
    - Red first on jupiter: the_platform_reads_a_kernel_sleep_blocked_and_a_spin_long read NoClock with main's
      account; green: sleep Blocked, spin Long, 291 ns a read. Linux (CachyOS 7.2.9) green, 210 ns a read.
    - Unsafe budget slates-machine 104 -> 105. GAPS: A-871's NoClock line closed.
  - For main: clippy and the Windows cross-lint for slates-machine and slates-rt; a jupiter round naming its long
    polls' holders.
  - Queue empty.
- 2026-11-23 ~10:30 PST: D'' for main's adacachy2 A/B, NOT landed: 71e7945fe on agents/w-perf-84a-linux-rr (on main's
  933ec74f8; B = 374ce0e8e).
  - A Linux client's reply wait holds SCHED_RR from its first park until it returns; restored once, exactly.
  - Red on adacachy2: a_reply_wait_holds_the_real_time_class_from_its_first_park_until_it_returns, 2 raises per wait
    with the per-park code, now 1, nice back exactly. machine and rt round_robin tests green there.
  - A-872 claimed (19e433784). Worktree w-perf-diag retired (clean, HEAD on origin's mac-tail branch); w-perf-rr made.
  - Also saved: Linux 7.3.3 fair.c reading in ~/.cache/w-perf/linux73-notes.md.
  - Waiting on: main's A/B of A, B, D'' (R9 provisioning p99, load throughput). Queue otherwise empty.
- 2026-11-23 ~11:30 PST: LANDED A-872, 015dd0f16 (arm B cherry-picked from main's 374ce0e8e) + 2e8ee75f7 (docs).
  - main's ruling on the adacachy2 A/B: B alone; D'' (71e7945fe, agents/w-perf-84a-linux-rr) measured, not taken.
  - rt round_robin test green where granted (adacachy2, adacachy), loud at RLIMIT_RTPRIO 0 (ulimit -r 0 on
    adacachy2), skipped loudly on the Mac.
  - Docs: §4.3 drivers, amendment A-872, GAPS (84a Linux: bare metal met), BENCHMARKS "84a on Linux bare metal" (A, B,
    D', D'', E; R9 median cost 12-20 -> 17-27 us).
  - Not run by me: clippy (Ada's §8; main's wall). Owed: main's confirming round on main.
  - Queue empty.
- 2026-11-23 ~15:00 PST (goal 93 memory term):
  - LANDED a472a4ca4: GAPS, A-872's RealtimeKit route parked (main's rulings recorded).
  - LANDED 5cc3f261b: crates/server/tests/hg_push_lifetimes.rs (#[ignore]d; sampled by site, x86_64 size class, life;
    SLATES_HG_PUSH_SOURCE=~/.cache/w-perf/hg-src, CARGO_PROFILE_DEV_OPT_LEVEL=2).
  - LANDED cf7c67662 (merge c9e130878): the store's TextCache in chunks of the mapped floor, the oldest given back
    whole; Store::make through two reused buffers; text_at still lends. Red/green tests/text_cache_heap.rs: 6,606,904
    live bytes grown -> 90,424. Push of Mercurial's history after: sub-floor >=1 s/live 4.18 GB -> 0.68 GB; live at end
    under floor 366 -> 189 MB; [128K, floor) churn 3.52 -> 2.72 GB.
  - Finding for main: the cache's bound still grows to the longest chain's bytes (no longer needed with the buffers):
    360 MB of mapped chunks held at the push's end.
  - Split with w-hg: intake::rebuilt, HgIndex::revision_text, Taker::file_base are w-hg's (C1).
  - Pushed once before rerunning after a merge that brought crates/mem/src/arena.rs; reran the store tests after: green.
- 2026-11-23 ~08:30 PST:
  - LANDED 0cd8ef36a + d0183b7c1 (merge 121dbf6b5): every hg append seals the index's tail when due through one
    HgIndex::insert; import's own seal gone (red: due after group 26 of 200; w-hg's three conditions). Plus the spare-
    chunk measurement (#[ignore]d): Mac 1 MiB chunk 240 us fresh vs 85 us kept; x86 not measured (adacachy run timed out
    building at 40 min).
  - REGRESSION found by main's arena-13 (CI 45193998198): b04cdef7f's for_reads(waiting) bound kept the distinct texts
    of hundreds of reads, 646 MB of chunks resident after push-all and idle.
  - LANDED cc95e92c4 (merge 67d3d866d): the cache holds the walks in flight, in whole chunks, plus one chunk; red 7.3 MB
    -> 1 MiB with room for 1,000 reads. Sampler strip fixed (::h + 16 hex, four frames).
  - Sent: w-shared inbox (image pages 22 MB); w-hg the revision_text frames (push::held::IndexHeld).
  - Owed: main's arena-14 on CI. Next per main: the content store's tables (3)(4); one rerun after w-hg's cut 3.
  - Worktree w-perf-rr removed (D'' on origin). Target dir was deleted by the disk cleanup.
- 2026-11-23 ~14:00 PST (session restarted; now slates-work-b9):
  - arena-14 (CI 45208924823) push-all killed the daemon on all legs: shard 0 spun >1 s in hg_push::segment. Cause:
    my seal 0cd8ef36a -- each seal a run, a push's fork never merges, so every new row's lookup walked every run.
  - LANDED 45de607e7: revert 2662ca69f; arena owner tags 79f9ae578 (mem.gauge.content.arena_<owner>_bytes/_asked);
    sampler container tags b582df491. Sent w-shared ChunkWindows numbers (216fbf7fa).
  - Next (main's ruling (a)): reland the seal with the push's runs merged a step at a time through index_upkeep's
    merge_step, size-tiered, run count bound in a Derived: line; red: runs walked per lookup under the bound over a
    long push, and one segment step within the quantum. Tell w-hg before touching crates/hg/src/index.rs.
  - Waiting: arena-15 (main) on 45de607e7.
