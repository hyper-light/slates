# w-hg -- UI goal 93 part "The Mercurial server built"; implementer; supervisor "slates-7a" (SendMessage to: "slates-7a")
# Policy read 2026-11-19: relaunch-2026-11-19.md, briefs/common.md, briefs/w-hg.md, restart-preamble-2026-11-18.md,
# slates CLAUDE.md §8 (origin/main b63f8a05d). Studio d1323898 via `kiln run --on d1323898... --no-sync -i -- 'bash -s'`.
# Tools: ~/.local/state/slates-fanout/w-hg-tools/ (run-2b.sh, messages). Studio: ~/.cache/g303-target (reused),
# ~/.cache/w-hg-logs, ~/.cache/w-hg-clone (deleted by script); delete g303-* and w-hg-* there at END.

## Piece 1: A-680 part 2b (hg push over HTTP) -- landing
- Worktree ~/slates-work/g303-a680-p2b-land2, branch agents/g303-a680-p2b-land2. Found mid-merge of origin/main
  b63f8a05d (daemon_edge.rs + design resolved, unstaged). Committed merge dfb40c810, pushed. Diff vs main: 17 files,
  the piece's alone.
- Run (2026-11-19, this agent): run-2b.sh over dfb40c810: hg --lib push:: serve::, server --lib vcs_http::
  vcs_store::hg_view repository::hg_push vcs_ssh::hg (4 threads), fmt, cli --test cli stock hg HTTP round trip
  (--include-ignored) + refusal. Output: w-hg-tools/run-2b.out.
- Relaunched w-hg 2026-11-19: r2 (Studio, over dfb40c810) hg --lib push:: serve:: 95/95, server --lib focused 78/78,
  fmt clean, cli stock-hg HTTP round trip + refusal --include-ignored 2/2 (ran, no skip). Un-ignored it (7c71c8048),
  merged main (only git_interop.rs + GAPS: no rerun). LANDED main 354b02d2a. amendments.tsv A-680 row updated.
- Owed: 2c (record 2b's pushes with A-687's kind) after A-687 lands.

## Piece 2: A-687 (durable import + the one Mercurial record kind) -- next
- Merged b63f8a05d then 354b02d2a into agents/g300-a687-import-durable (cdc0b2987; push.rs Stored = Git+Imported+Lfs,
  ReplayRefused::Owed dropped). Squash-landed via ~/slates-work/w-hg-a687-land (agents/w-hg-a687-land): piece
  cd5217d1f, LANDED main 6a876f6ae. Studio r4/r5: hg --lib import 28/28, server focused 84/84, fmt clean.
## Piece 2c: A-680 part 2c (record hg pushes with SHGR Source::Push; replay) -- next
- 2c LANDED main 243b7bfae (piece 23e2789e7; worktree ~/slates-work/w-hg-a680-2c). Studio: hg --lib import 28/28,
  server --lib repository::hg_push vcs_store:: repository::push repository::import 71/71 (new
  a_landed_push_is_replayed_into_the_store_from_its_entry). Remote branch agents/w-hg-a680-2c holds the pre-fmt
  a1a2632dd (amended locally after pushing; not force-pushed).
## Piece 3: A-696 hg push over SSH (g304's branch) -- next
- A-696 LANDED main 3691deb5c (one commit on origin/main; worktree ~/slates-work/w-hg-a696-land). g304's commits
  cherry-picked, tests fixed for main (ToolServices 4 args, borrow fixes). Studio: hg serve:: 63/63, server vcs_ssh
  vcs_http::hg hg_push 81/81, stock-hg SSH round trip 1/1 -> un-ignored. g304's worktree left as found (mid-merge).
## Piece 4: hg `lookup` (briefs/g258.md) -- next
- A-705 hg `lookup` LANDED main 3bcaccf91 (piece e783c3440; worktree ~/slates-work/w-hg-lookup). Local (this Mac,
  ~/.cache/w-hg-local-target): slates-hg serve:: 66/66 (3 new lookup tests), cli stock_hg round trips 2/2 with
  `identify -r tip`. Stock 7.2 answers recorded on the Studio. Owed: tags beyond tip.
## Piece 5: stream clones parts 1-3 (briefs/g083.md; progress/g083-w1-under-1ms.md) -- next
- A-706 stream clones part 1 (slates_hg::render) LANDED main 3fee2f575 (worktree ~/slates-work/g083-hg-stream-p1,
  branch agents/g083-hg-stream-p1). Local: slates-hg --lib import 29/29 (new round-trip test); stock 7.2 hg verify
  passed on rendered dot + states (one-off, uncommitted writer). Owed in part 1: deltas/compression, render as
  versions commit (sliced).
## NEXT: stream clones part 2 (`stream_out` over render) then part 3 (stream2/streamreqs, stock `hg clone --stream`
  over the daemon edge + hg verify). Worktrees held: ~/slates-work/w-hg-lookup (landed; removable),
  ~/slates-work/g083-hg-stream-p1. Local target ~/.cache/w-hg-local-target. Studio: ~/.cache/g303-target,
  ~/.cache/w-hg-clone, ~/.cache/w-hg-logs, ~/.cache/w-hg-run-2b.sh to delete at END.
- END (context short) 2026-11-20: Studio dirs (g303-target, w-hg-clone, w-hg-logs, scripts) and the local target
  ~/.cache/w-hg-local-target deleted. Worktree ~/slates-work/g083-hg-stream-p1 kept (landed, clean) for part 2.
  Part 2 plan: serve `stream_out` (7.2 generatev1: `0\n`, `<files> <bytes>\n`, per file `<encodedir(name)>\0<size>\n`
  + bytes) over render's files through the server's lender (HgStores needs index+content access for the render);
  advertise `stream`/`streamreqs=generaldelta,revlogv1,sparserevlog`; then part 3 stream2 + stock `hg clone --stream`.
- 2026-11-20: A-707 stream part 2 (`stream_out`, streamreqs) committed on agents/g083-hg-stream-p1 (pushed, NOT landed):
  local hg serve:: import 96/96. Before landing: stock `hg clone --stream` over the daemon edge + hg verify; docs
  lines; A-707 row (not yet claimed in amendments.tsv).
- PAUSED for main's TOP PRIORITY: hg push of Mercurial's own repo (57k changesets, 84 MiB) -> 413 (unbundle body past
  the request's room, vcs_http/hg.rs). Fix as A-704 did for git, sliced; failing tests first; proof w-perf's
  crates/cli/bench/hg_scale.sh (branch agents/w-perf-p2); tell w-perf (slates-work-89) when landed.
- A-711 (hg push room = shard's admittable, HTTP+SSH; past it pusherr) LANDED main bc2803d66 (worktree
  ~/slates-work/w-hg-a711). Local 24/24 incl. new daemon_edge large push test (red 413 first). Told w-perf to rerun
  hg_scale.sh. A-712 claimed for stream part 2 (commit ed54ff9b1 on agents/g083-hg-stream-p1; the A-711 commit there
  too, cherry-picked to main -- re-merge before landing).
- NOW: import room (ImportRefused PastRoom{278528} on Mercurial's repo; hg_import::room_of = one region, reserved as
  retention at begin). Derive per repository, sliced (the import is already turn-sliced).
- 2026-11-20: w-perf rerun of A-711: 413 gone; 57k push trips watchdog (1152 ms). Measured hg repo manifests: 57,007
  revs, 9.4 MB deltas, 5.5 GB full texts -> slates' full-text storage (index content, push Verified, SHGR records) is
  the blocker. Sent main a 4-piece proposal (delta storage, delta records, sliced intake, per-repo import room);
  awaiting word; starting (4)/(3) meanwhile. One-off import least-room measurement running (bg task byk42vq42).
- A-712 stream part 2 (`stream_out`, streamreqs) LANDED main 7405f9508 (piece f78ff53f0; worktree ~/slates-work/w-hg-a712).
  Local hg serve:: import 96/96. Owed: stock `hg clone --stream` over the edge (part 3 with stream2).
- 503 tests fixed (helper lands the import; heads = stock served heads) LANDED main (piece 3a59b5776); daemon_edge 13/13.
- Main's order: (3) sliced intake + (4) per-repo import room, BUILT with content-once: decode each revision as it
  arrives, rebuild against a stored base, check node, store once; file texts named by the green's ContentIdentity;
  working set bounded independent of push size; admission reserves only the working set, content charged as stored
  (A-714 retention); cite w-git's §4.17.1 rule. Then (1)+(2) manifest/changelog deltas + SHGR v2. Numbers: claim by
  pushing amendments.tsv on agents/fanout-state (next free A-720 at last word). One-off import-room measurement was
  killed at 30 min (release build + 5.5 GB of manifest texts); not rerun.
- A-720 claimed (ledger pushed). Part 1 (slates-hg push::intake, sliced engine) LANDED main (piece 56952183f). Local push:: 41/41.
  Note: ran `git checkout -b agents/w-hg-a720` in ~/slates-work/w-hg-a712 by mistake (rule breach; no files changed).
- NEXT part 2: server drives Intake in slices (with_state per slice, yield between, quantum = step_budget_ns), one
  intake per repository at a time, prepare/classify/tree over IndexHeld(fork), land with the fork, record from fork.
  Storage: point at w-perf's identity table when it lands (crates/merge slice 1); no second store meanwhile.
- A-720 part 2a (server drives the intake in slices, async HgStores::push, own fork per push, generation stamps,
  landing judged at landing, record from fork) LANDED main (piece 6e1896f22, merge 49439c77e). Local: hg push:: import
  71/71, server focused 44/44 (incl. after final merge). Removed ~/slates-work/w-hg-a711; cleared target's release +
  incremental (disk was 17-18 GB free).
- NEXT: part 2b (re-base a stale fork by node at landing instead of MOVED), then part 3 (import room per repository),
  then S5 per main's convergence plan (file texts as green files by identity, changelog/manifest deltas, rows by id).
- A-720 part 2b (stale fork re-based by node at landing; land never swaps in a stale fork; side-by-side pushes land)
  LANDED main (piece 64d457979, merge 5df3c5a18). Local: hg 72/72, server 45/45 (32/32 hg filters on landed tree).
- NEXT: part 3 (import room per repository; ImportRefused PastRoom{278528} on Mercurial's repo), then S5.
- A-720 part 3 (import room = shard's admittable, charge trued up to Import::holding each turn) LANDED main (piece
  08d45661e, merge 99c0f7877). Local: server import/hg 15/15 + 40/40, hg 72/72. By-use proof owed to w-perf.
- SSH test fix (stock push over SSH awaits steps; was "a step that waits") LANDED main (b1ec1fe00, merge 7477a94a7);
  server `-- hg` 54/54 on landed tree. NOTE: use filter `-- hg` (my `vcs_ssh::hg` missed vcs_ssh::tests::hg).
- Target trimmed: 24 GB -> ~1.3 GB (incremental, release, orphaned *.rcgu.o); disk 78 GB free.
- QUEUE NEXT: S5 (main's convergence plan; main will send hg's step details), then the server-made hg changeset
  (hg_made::make hook in verbs::judge_submission, per w-shared's seam message), then the inventory list.
- A-731 (server-made hg changeset: slates-hg push::made, vcs_store::hg_made, Source::Made, HgStore::made, head_of)
  LANDED main (piece 9ed874650, merge 36c65b2a3). Local: hg 74/74, server `-- hg vcs_store:: repository::` 130/130.
- NOW: S5 = A-729 (claimed): content once by identity (main's 6-point spec: file texts by identity homed via homes.rs,
  read via A-727 Cow seam; manifests/changelogs as revlog-shaped deltas, snapshot interval derived; SHGR names
  identities only; intake rebuilds against stored base; replay re-indexes by identity; proof w-perf's import).
- A-732 crash-loop fix (record::Replay + record::Writer; recovery replays hg records a revision a unit; import's
  Stage::Writing) LANDED main (piece 165bcd2a7, merge e7353d339). Bug record docs/bugs/2026-11-20-a-large-mercurial-
  import-crash-looped-the-daemon.md. Local hg 74/74, server 131/131 (+ flaky git test t_a664_live... passes alone).
- NEXT (main's order): (2) the whole 57k push over HTTP still spins shard 0 ~183 s in (w-interop, release 1533d52b,
  RSS 13.9 GB): find the unsliced step (landing/record write/classify), failing test first; then (3) S5 (A-729).
- A-732 part 2 (push record written in slices before landing: record::Writer::begin_kept, hg_push::record_sliced)
  LANDED main (piece d8b23d0e5, merge 5a917afcd). Local hg 75/75, server 136/136 (61/61 on landed tree).
  Still one step: the landing's append of an entry as large as the record -> S5 (A-729) fixes by identity-only records.
- NOW: S5 (A-729).
- A-729 part 1 (manifests kept as deltas in the index; delta::encode_lines; HgIndex::revision_text) LANDED main
  (piece 2a42df233, merge d7713a435). Local hg lib 354/354, server 137/137, cli stock_hg 4/4.
- NEXT: (1) slice vcs_store::rebuild's replay (post-recovery spin per w-interop rerun); (2) S5 part 2 (SHGR v2:
  names + changegroup deltas; replay re-intakes).
- A-732 part 3 (sliced Mercurial rebuild: VcsStores::rebuilding, replay_sliced task; rebuild_whole for git/svn/tests)
  LANDED main (piece 4fa835b3f, merge 0f756ec77). Local server 140/140.
- NOW: S5 part 2 (A-729): SHGR v2 = names + changegroup deltas; replay re-takes it through the intake.
- 2026-11-20 (after compaction), LANDED on main, each with focused tests (target now ~/.cache/w-hg-local-target.noindex,
  CARGO_INCREMENTAL=0):
  - A-729 part 2 push: SHGR v2 = changegroup as arrived, replayed through the intake (b1d001601). Proven by use:
    w-interop 57,035-rev push 142 s, RSS 5,460 MiB vs 13,450, no spin.
  - A-729 part 2 import: SHGR v3 manifests as index deltas (5ad8dea09); made records v3 manifest delta (669bf99e4);
    import file revisions as one-hunk deltas vs previous rev (de49b6bd4; w-perf found 4.66 GB record + one-step
    landing append spins shard 9; C run queued). OPEN (main's ruling asked): bounded import append (entry parts, or
    identity-only records after A-727 a3).
  - A-575 sibling: hg branch verbs name bookmarks (62fb88997).
  - A-745 (claimed): fetch answer in slices. Design rule §4.17.1 + red (3daa8b369); piece 1 Answering/Walk by links
    (d0749fe2d); 2a HTTP streamed (658eb7d9c); 2b SSH (2aeeac954); rule 4 generation hold (216c57ad2); piece 3
    stream_out streamed (84a596819); stream2 findings in GAPS (f14562e4c). Owed: stream2, matrix rerun (w-interop).
  - A-731 copies in made changesets (eeabd1dfe).
- OPEN with main: S5 homing design (homes.rs first consumer + async read path) — asked, awaiting ruling.
- NEXT: inventory by user impact: pushrebase (server-made changesets over a moved head), clone bundles, ...
- 2026-11-20 (later), LANDED: ssh-clone stall (rekey with a full outbox; key time fixed at RFC's 1 h; bytes-driven
  volume rekey proved under ChaCha20, 53a81211d); identity-only import records SHGR v4 (306214c57).
- Item 5 (A-575 `--from`, main's ruling): Mercurial lookup order (bookmark, tag, branch tip) via
  slates_hg::serve::lookup::resolve_name; slates_hg::tags (7.2 fold, 300 vectors from 7.2's own functions);
  served lookup resolves tags. Commit e531594fa, merged with origin/main 2cf1b1d35; focused tests rerunning as kiln r24.
- Item 2 clone memory: r21 big Studio, base 359da2388: push peak 7,687 MiB, after-push 8,109; first HTTP clone ->
  12,143 MiB, held after; ssh and stream clones add nothing. r23 (vmmap + heap per phase, second HTTP clone) running.
- Local target deleted (main, disk low): every cargo run now through kiln, no `on`.
- NEXT: item 2 bound (red first), item 6 (forced new-head push over a moved line), item 4 S5 head texts.
- 2026-11-20 afternoon, LANDED (each red first or held by a stock oracle; focused tests local):
  - 46951a489: lookup and every whole answer get the fetch's room (clone-rev's PastAdmission).
  - 648a46d1a: the changeset A/B example plus its BENCHMARKS section (counts and release timings).
  - 75e2d3fb4: A-732 part 4, the intake hands each manifest append its predecessor's text (no chain rebuilds).
  - dc59ef2a1: revision_text applies a chain into two swapped buffers.
  - 6935d8e86: A-732 part 5, a replay unit takes steps until its slice is spent (vcs_store.hg_replay_turns).
  - 13523dbe3: item 6(a) proven, a forced new head on a moved line held to stock 7.2's push -f.
- OWED: the restart of hg's history measured again, release (r42 queued on adacachy; the Studios are unreachable
  for sync). Item 6(b) (the lock-free rebase) waits on main's ruling. The changeset shape waits on w-store's arm.
- 2026-11-20 evening, LANDED:
  - f98d99685: a clone sends manifests as their stored deltas.
  - bab805323: whole texts written into the answer once.
  - Branch agents/w-hg-file-deltas-held (c9c3ef73b): the per-file arm, for w-store's A/B.
  - Inbox notes answered: w-store's stored form, w-sizing's numbers.
- WAITING: r42 (the restart and clone measurement, queued on adacachy; also in run-requests.md); main's ruling on
  item 6(b); w-store's piece 3, before hg moves onto the changeset store (the C item, with R2's red test: an import's
  committed content against the .hg's bytes).
- 2026-11-20 ~19:00, C1 (main's 18:30 ruling: hg's side against the seam, the store behind a double; land on main
  once w-store's wiring is there). Branch agents/w-hg-c1 (worktree ~/slates-work/w-hg-c1), each red/oracle-tested,
  slates-hg lib 376/376:
  - 446207514: the intake names each kept revision for the Texts section (path, node, text BLAKE3 and length, base
    identity, place in the payload).
  - 1420ec376: record v6, a push's record is its head alone, the changegroup the Payload section, replayed in place
    (Replay::begin_sectioned).
  - 2d6d17db0 + 287fc769a: crate::landed, the ChangesetLog seam, land_push (Record + Payload), Replaying a unit a
    step; refusals by kind (Missing, Record, Unreleased).
  - Next on C1: the server adapter (slates_store::Store as ChangesetLog) and hg_push::land / recovery onto it, once
    w-store's wiring lands; the Texts section once w-perf's encoder lands.
- R2: r43 placed itself on this MacBook (load 116) and was stopped (no measurement here). r42 is still queued on
  adacachy.
- ~20:50 C1 on branch agents/w-hg-c1: 3ffb95c4b (land first, v7 reference in the entry, replay from the store by
  piece), a9620fbb1 (Texts section). By-use test blocked: the store refuses every landing on this Mac (memtable past
  4 GiB; reported to w-store). Lands on main when it passes.
- ~21:20 LANDED 242f4a173: C1's push onto the changeset store (by-use test with stock hg 7.2 passes; suites green).
  NEXT: the import onto the store (rest of C1; R2's peak).
- ~21:45 main's ruling (inbox/w-svn-from-main-c2-rulings.md, decision 1) applies to C1: no repository without
  --vcs-store (a typed refusal naming it), no register-held history path for hg, and tests on store-backed daemons
  through ONE shared helper (repository::store_tests' directory, made shared with a removal guard). Not small for hg:
  every hg server test runs on a store-less shard through the sync push path (unbundle_in). Plan: the very next
  commit after C1 lands. Recovery rule already matches decision 2 (next sequence = named + 1; unnamed changesets
  dropped).
- C1 remaining order:
  1. file texts read through the store (T rows) instead of the content store;
  2. the intake keeps no whole text with a store;
  3. the import as changesets (R2).
  Then the no-store path dropped.
- 2026-11-21 ~00:20 LANDED 8cb10f986: C1 step 0, the no-store commit. One `repository::no_store_for` refuses hg and svn
  without --vcs-store. The v2 path is gone. hg tests are on store-backed daemons through the one shared helper
  (w-svn's test_store.rs plus mine: audit_on_stored_shard, land_blocking, create → repository::create_blocking).
  Close note written, with the siblings: 10 failures already on main 68ce3d585 (whole_build's park bound) and the
  load-bound git_push t_a752.
- ~00:30 asked main (inbox/main-from-w-hg-c1-order.md) to order C1 as 3, then 1, then 2: step 1 alone can't serve an
  imported repo, whose texts aren't on the store until 3.
- ~00:45 step 3 design sent to main and w-store (inbox/main-from-w-hg-c1-import-design.md): the import lands as a run
  of bounded changesets whose payloads, concatenated, are one cg3 changegroup; the register's one import entry names
  the run; replay goes through the push intake reading pieces.
- NOW: the piece-reading intake (needed by both 1 and 3).
  - slates_hg: a `Source` (bytes by range, or Wanted{range}) and a forward `Window`.
  - The changegroup framing decoded in steps from a cursor over a Source.
  - Intake::step reads its delta through a Source; a step asks for its bytes before changing any state.
  - Server replay (vcs_store replay_hg_referenced / replay_sliced, payload_of): a window filled by
    `StoreHandle::piece` on Wanted. No whole payload is held.
  - Red test: a replay's peak bytes against the payload's length.
- 2026-11-21 ~00:55 LANDED d571ffbca: the piece-reading replay (framing over a Source, intake Wanted, Replay piece
  mode, server fill_window; red test 789,956 B payload in 14 fills, window at most 193,859 B), and the daemon_edge hg
  tests green via w-git's C5 part 2a. Close note written. Waiting on main's ruling for C1's order (3, 1, 2) and the
  import's shape; next by default: step 3, the import as a run of changesets.
- 2026-11-21 LANDED 6a0600ff1 (segment records v8, run reference v9, main's oracle on all five 7.2 formats) and
  a693c49e4 (C1 step 3 server half: the import lands as a run of segment changesets; recovery and rebuild read the
  segment records, 0 payload reads; imports stream). main confirmed. The push memory work went to w-perf whole;
  t_a752 is w-bugs'.
- C1 step 1 (main approved 08:45, inbox/main-from-w-hg-c1-step1-design.md, with the pull condition): getbundle's file
  revisions come from the changeset store through w-perf's render::StoreWants (Want::Rows via texts_by_tool,
  Want::Chunks via places), answered by hg_view::answer_store_wants between turns (vcs_http/hg.rs:653,
  vcs_ssh/hg.rs:184).
  - A stored delta is sent as it stands when its base is known by node (a revision sent earlier in this answer, or p1
    or p2 whose link is served). Ask rows for (path, [node, p1, p2]).
  - Otherwise send a whole text, made by the store (`StoreHandle::text`), against null: a new Want variant beside
    w-perf's; tell w-perf.
  - Counters: whole texts made; revisions sent whole.
  - main wants a measurement of a one-changeset pull touching a large file, slates' bytes against stock's.
  - Plan:
    - 1a: the by-links walk (clone): Walk::file in serve/getbundle.rs, plus Producing::Bundle's StoreWants in
      serve/session.rs.
    - 1b: the pull path (named_changegroup, built whole and synchronous today) rebuilt as a stepwise walk with wants.
    - Then lookup's .hgtags, the intake's base text, made changesets.
    - Then step 2, after w-perf's intake change lands.
- 09:00 main approved: made changesets onto the store FIRST, then 1a (the clone walk), then 1b (the pull).
  Conditions:
  - ONE HOOK: made_commit (A-804/A-805, Lineage::preview), the one svn_made uses. Ask w-rewrite-ops if it lacks
    anything.
  - ONE PATH: the server-built changegroup lands through the push's changeset path (Record v6, Payload, Texts, v7
    reference). In the same series, remove the v3 made record (record::write_made, the DELTA_VERSION reader/writer, the
    `formed` code), hg_made::recorded, and HgStore::unstored, with no fallback.
  - Red: a repository with a made changeset is refused by the store rule today. Green: it clones and pulls through
    the store, stock 7.2 verifies clean, and a restart replays it.
  PLAN:
  - M1 (slates-hg):
    - record: `write_made_record(room, version, (green, ver))`, a SECTIONED head with Source::Made. head() lets
      SECTIONED/REFERENCE take Push|Made.
    - begin_sectioned keeps the made (green, ver); Retaken finish sets Replayed.made = (green, ver, the first new
      changeset's node).
    - Reference gains `made: Option<(u64, u64)>` (write_made_reference; reference_of; record() picks the made record).
    - `push::made` encodes a Verified as a cg3 changegroup (put_revision_whole per revision; groups changelog,
      manifests, empty tree list, files).
  - M2 (server):
    - made_commit::defer passes `judged` to hg_made::make_deferred.
    - hg_made builds the Verified from a preview (svn_made::revision_held's pattern; changeset_of takes the engine), and
      the bare make() spawns a task as svn_made::make does.
    - The task: encode the cg, take it through the push intake into a fork (hg_push's begin_intake/take path),
      hg_push::changeset_of with the made record, `disk.land` awaited, then made_commit::commit with landed_on_disk,
      the register entry (v7 made reference), the fork placed, and the made map set.
    - The recovery/rebuild replay of v7 already goes through the pieces; replayed_hg sets `made`.
    - Tests poll for the made changeset (async now).
- ~10:10 LANDED ac25475c1: made changesets on the store (one hook, one path). The v3 made record, `unstored` and
  `stored_logs` are removed. Close note written; main and w-perf told.
- NEXT: C1 step 1a, the clone walk (by_links) in serve/getbundle.rs Walk::file through StoreWants.
  - Walk fields: want, wanted logs, the ahead deque (path, position, node, link, row, chunk), and a sent map
    (identity -> node) for the open group.
  - A new `Store::file_entry(path, node) -> ([Node;2], flags)`, read from the index, no text.
  - A stored delta is sent as it stands (put_delta against the sent node, or NULL for a stored whole).
  - Producing::Bundle forwards wants to Answering, then the Walk.
  - The edges already answer wants between turns (vcs_http/hg.rs:653, vcs_ssh/hg.rs:184).
  - Whole callers (getbundle::changegroup, session::serve whole, the tests) need a want-answerer: render's StoredSource
    / answer_want style.
  - Red: a full clone makes no whole text and reads chunks = file revisions; stock 7.2's clone verifies clean.
  - Then 1b (the pull path, named_changegroup) with main's condition: the base on the client or sent earlier, from the
    common set; count whole sends; measure a one-changeset pull touching a big file against stock.
- ~11:20 LANDED 5d2940ff9: C1 step 1a.
  - The by-links walk (serve/getbundle.rs Walk::file) reads file revisions through StoreWants (Files: ask_rows /
    ask_chunks / give) and sends stored deltas as they stand.
  - Store::file_entry and Store::stored_now (test stand-ins; the daemon's Indexed answers nothing at once, and its
    test seam is `answering_from_its_texts`, cfg(test)).
  - HgStores::answer_wants: ServedViews overrides it to hg_view::answer_store_wants, and the default answers from the
    lent view's stored_now.
  - test_store::answer_wants_blocking.
  - hg_interop's stream test does a plain clone that asks the store.
  - Target trimmed for main (27 GB free).
- NEXT 1b, the pull path:
  - named_changegroup (getbundle.rs ~910: common_ancestors, changelog_entries, named_revisions, encode_changegroup) is
    built whole at begin from content-store texts.
  - Plan: keep the planning (which revisions, by path, node, parents, link) but send each file revision from stored
    rows and chunks, stepwise.
  - Ask rows for (path, [node, p1, p2]). The base is sendable if the stored base identity is a revision sent earlier in
    the group, or p1/p2 when the parent's link is held (on the client) or sent.
  - Otherwise send a whole text: a new Want::Texts variant answered by StoreHandle::text pieces (render.rs Want and
    answer_store_wants are w-perf's: tell them), counted WHOLE_SENT.
  - Measure a one-changeset pull of a large file, slates' bytes against stock (main asked).
- LANDED 1b b9fed44e9 (base 5d2940ff9): the pull's file section is serve::getbundle::Pull, read a step at a time from
  the changeset store (rows, then stored edits, then Want::Texts one at a time). A stored delta goes where its base is
  earlier in its group or a parent the common set holds; otherwise the whole text against null, counted
  (Walk::whole_sent, vcs_store.hg_pull_whole_texts). Measured: a one-changeset pull of a 2 MB file is 554 bytes, the
  same as stock 7.2, none whole. Main confirmed; w-lint runs clippy and w-interop the full hg_interop.
- WAITING:
  - main's ruling on lookup's .hgtags: A, wants at each of the four entry points including the --from verb as a task
    (my lean); B, all .hgtags texts kept in RAM; C, .hgtags exempt from step 2;
  - w-perf's answer on intake.rs ownership for the intake's base text (inbox/w-perf-from-w-hg-intake-base.md).
- Target trimmed by exact paths (4 stale rlibs and .o files, 6.9 to 6.2 GB).

## 2026-11-22 ~05:00 PST (w-hg, main 01a20182)
Landed today: aff81b23b (daemon-edge reds, Works::Built uses), d93f98369 (C1 2a read-through, NotYet), 14b00683a,
ed5c20b9d (2b part 2a: away windows ride the image), e765b8245 (LengthDiffers), 2d1399d2c (PastAdmission part 1,
retakeable intake), ecf8840d7 (part 2, RoomWait), 858908e2c (wall), b624f67d6 (part 3 (a), known need admitted),
dc1a69fa0 (wall for (a)).
In flight, ruled (b1) by main 04:44: the push lands as a run of segment changesets as the import does (segments >=
hg_import::segment_bytes, ordinary bounded landings between the intake's turns; entry names the run, v9; made path
keeps v7; conditions: space of an abandoned run comes back, crash oracle at each segment boundary, admission = a step +
one segment, peak RSS before/after).
Held as patches (not landed): ~/.cache/w-hg-2b-1-works.patch (2b (1): the import's bookmark works built a unit a turn
before its landing; green; owes its non-vacuity test that the landing took more than one turn);
~/.cache/w-hg-2b-letgo.patch (2b: the import lets each segment's file windows go once durable; needs 2b (1) first).
Not reproduced, logged: CI's cold-repository "push again" (log line "a push's increment held ahead was refused by the
content store"), a wait's give-up ("a push's intake waited for room and gave up").

## 2026-11-22 ~11:20 PST (w-hg, main 01a20182)
Landed since 05:00: 47edbe430 (take_sliced split, intake cutting, widest_full_text), faa87e583 (v9 push run reference,
wall sites), b61a5934f/746f4753f ((b1) segment runs + (i) landing turn; Rebasing removed; crash oracle; head-of-line
537 ms measured), 89d75e30b (manifest entries kept once; bound linear), 5c15d3e8a (v10 push segment record, base by
node, replay over a moved index).
Now (main 10:40, the spin before staging): stream row gathering sliced a content window a step (render::rows_held;
red a_streams_ask_for_a_long_logs_rows_is_gathered_a_window_a_step proven; hg 416/416; not landed, server tests next).
Running: daemon_edge diag_the_longest_step_of_each_read_a_clone_of_a_long_history_makes at 57,054 changesets (debug).
Suspects next: heads/branchmap (one step over every revision), getbundle named_changegroup (clone -r, every missing
changeset's entry at once).
Then: staging pieces (bounded stale re-base; store staging key kind; forgetting an abandoned key's rows), and a push's
texts let go once their segment lands (RSS 5.5 GB; asked main for content_range.*.resident after push-all).
Held as patches still: ~/.cache/w-hg-2b-1-works.patch, ~/.cache/w-hg-2b-letgo.patch.

## 2026-11-22 ~17:55 PST (w-hg, main 01a20182)
Landed since 11:20: 7ced987e6 (clone spins: getbundle::Named a unit a step; stream ask gathered a window a step),
e72a3d97f (stale re-base equals v10 replay), c2ef17856 (store: forget an unnamed changeset's rows, K keeper rows),
37efbdab6 (staging: segments off the turn, v11 ranges, bounded re-base, abandoned runs forgotten), 62d01f9a8 (RSS:
landed push's file texts let go, bring-back within budget; v9 push reader dropped), 50f62ead2 (two load-sensitive
staging tests bounded by what they measure).
CI 45065911852: 24/24 on three legs, clone spins gone. RSS: arena 4.7 GB held after push-all (texts); let-go landed,
main reruns. Open for main's ruling: landed_pin keeps 1/shards of texts (all on a one-shard node).
Next: 2b (1) (~/.cache/w-hg-2b-1-works.patch, owes the >1-turn non-vacuity test), then 2b let-go for the import, then
changelog/manifest off the content store.

## 2026-11-22 ~19:40 PST (w-hg, main's ruling of 18:43; w-shared's seam 1)
Landed ee1c9005b (piece 23e2789e7): `hg_push::bring_back(repository, (path, node)) -> bool`, the hg half of 2b(b),
part 1. It reads the row, asks the changeset store for the text off the step (`slates_hg::push::held::TextAsk`, the
work builder's ask moved there), and puts it back within the lent room. Counter `vcs.hg.landed_texts_brought_back`.
Test a_landed_file_revisions_let_go_windows_are_brought_back_whole_by_its_path_and_node: red via a stub (49/120 let
go, none back), green. Marked `cfg_attr(not(test), expect(dead_code))` until homes' Hg arm calls it.
w-shared's types landed fc42f581c (note_landed, Landed::Hg, HgLanded).
Next, in main's order (18:5x):
(1) After w-shared's region-object hold fix: hg's red at the edge. A landing whose file shares a window with an
object another repository holds must have its release free only what the push owns (codex-review's
region-object-pin note).
(2) Then seam part 2, in one landing: note_landed per window (object_layout) in let_go_windows, plus the Landed::Hg
arm in homes::bring_back_landed awaiting bring_back per distinct key. Drop the expect. The let-go reds flip to every
window let go.
(3) Then the changelog/manifest move off the content store; main reruns the arena lane after the pin goes.

## 2026-11-22 ~21:30 PST (w-hg)
Landed 6b36e0269. Two pieces, each red-first:
- 139fc4e9f, the intake's refused finish (main's routing of w-svn's sweep). A refused node check or finish_object
  dropped the writer with its windows held; both now abandon it. Red (true, true, 2), now none held.
- a401e9c51 plus the WIP commit before it, seam part 2 (hg's half of 2b(b)).
  - Segments carry FileContents (path, node, ids).
  - The let-go after a push's entry commits or an import lands notes each window as Landed::Hg and lets it go,
    pinned by nothing.
  - The let-go at segment durability notes nothing: bring_back reads the committed index.
  - homes::bring_back_landed has its Hg arm.
  - Test-only: Works::Whole replaced by plan, then works_whole (blocking read-through), then Works::Built.
    prepare takes the view alone.
  - Red-first edge test: a region object sharing a landed file's window is read whole from its home.
  - The let-go tests flip to every window let go (were 71/120 and 1/5 pinned).
  - Focused: server 77/77, hg 133/133.
Next: main reruns the arena lane (the pin is gone), then the changelog/manifest move (replay and intake first).
Main's heavier asks: CI hg matrix plus the arena diag at 6b36e0269 (push-all arena and RSS; landed_texts_brought_back).

## 2026-11-22 ~22:00 PST: plan to the bar (Ada via main): peak RSS ~410 MiB on the 57,054-changeset matrix
Numbers are from CI 45101827617 (before 2b(b); arm64, after push-all) unless marked "est." (my estimate, to be
replaced by arena-3's read at 6b36e0269). Each term is held at the peak; the budget share is what it must fit.
Ordered by bytes now:
| # | Term | Now | Share of 410 MiB | Piece |
|---|------|-----|------------------|-------|
| 1 | Content arena, shard 0 (file texts pinned, plus index rows, changelog, manifests) | 2,535 MiB allocated | <=120 MiB | 2b(b) landed: pinned texts go after commit. Remainder = rows + changelog + manifests (terms 4-6) |
| 2 | Allocator slack: heap freed but resident, arena mappings reserved | heap 1.0 GB reserved / 435 MB used (~600 MB slack); 41 arena mappings 2.89 GB reserved; idle RSS 3,742 MiB | <=40 MiB | Give pages back when a push, clone or import ends (the allocator's purge / decommit of empty arena runs). Needs main's read of resident vs allocated per mapping |
| 3 | Heap in use, unattributed | 435 MB | <=80 MiB | Attribute first (main's heap profile at the peak and after push-all), then cut whatever dominates |
| 4 | Intake per-push structures, whole push (Run.contents FileContents ~112 B + path per file revision; Taker brought/awaited/positions/manifest_of; Intaken changesets/logs) | est. ~100+ MB in push-all | <=30 MiB, bounded by a segment | Bound per segment, not history: no FileContents list (walk the fork's file rows at commit to note); awaited checked a window a step against the fork; brought/positions shed as segments land |
| 5 | Manifests in the content store | est. 50-300 MB | <=60 MiB | Changelog/manifest move off the content store (replay, then intake, then works/made/verify, then getbundle, then kept_as/Rebase), only if arena-3 shows they dominate term 1's remainder |
| 6 | Index rows and runs (all logs' rows in content store windows) | est. ~60 MB (~150 B a row) | <=60 MiB | Measure; if over, page cold rows out to the store, as texts are |
| 7 | Changelog texts | est. ~25 MB | in 5's share | With 5 |
| 8 | Segment working set (payload slice + its texts) | one segment_bytes, est. a few MB | <=10 MiB | Already bounded by segment_bytes |
| 9 | Clone encoding (getbundle Named a unit a step; stream rows_a_step) | est. <10 MB | <=10 MiB | Already bounded a step; confirm the peak during clone rows |
Order of work: arena-3 read -> 2 (slack, the largest lever after 2b(b)) and 3 (attribution), with main's measurements -> 4 (push bounded by its work) -> 5/7 if they dominate -> 6.
Flake (main's 22:00): a_cleanly_restarted_import_brings_back_no_named_text. checkpoint_green defers to a
checkpoint_in_turns task under load and the test stops first, so the pre-pass rightly brings back 13. A test/doc fix;
sent to main and w-svn.

## 2026-11-22 ~23:30 PST (w-hg, main's URGENT on arena-3 run 45110563116)
- Landed c01aac8f8 (merge 18983677e): the commit-time noting and let-go is taken a turn at a time
  (hg_run::let_go_committed, half the step quantum, a revision its unit). Push: after its entry commits. Import: after
  its landing is durable. Red: 1 turn of 642,500 ns, now 120. Caveat: one revision (40-63 us debug) exceeds the 6 us
  quantum, so a turn is half the quantum plus one revision.
- Landed 29561c25e: store keeper_key by checked splits; EntriesOf alias (lint wall at dc549ba48).
- Open (sent to main and w-shared): the restart spin (serve_loop poll 2, 1,040 ms). Lead: vcs_home_pieces and
  restore_landed are O(notes) in one call.
- Next: the memory plan when arena-4 lands (w-shared's terms: the push peak is the rendezvous pin until commit, and
  ~1.3 GB stored after the push is unexplained).

## 2026-11-23 ~00:10 PST (w-hg)
Landed 6b7a4982f: a landed hg window is noted only where landed_pin would pin it (held for the region), per w-shared's
1ecb57cf4 (rendezvous pin gone) and main's ruling. Red: the let-go tests assert no notes (every window was noted).
9f42d8942: unused test bindings dropped. w-shared told of its 3 warnings (daemon.rs:8215/8256, homes.rs:75).
Waiting: arena-4 (1baba966b) rows from main, then the 410 MiB plan.

## 2026-11-23 ~01:30 PST (w-hg, arena-4 CI 45117277400)
- Landed d0fe61d57 (merge e7e0e4fa3): getbundle's file sections (clone link walk Files::ask_rows; pull Pull::ask_rows)
  ask at most Store::rows_a_step rows a step; the link walk counts rows scanned. That was arm64's 2 s step. Red: one
  ask of 40 and of 78 rows, now within 6, changegroups unchanged.
- Plan order agreed with main: the push-all peak first, judged by the push-all row's VmHWM, with
  vcs.hg.push_held_high_bytes as the non-vacuity gauge; a test drives two histories of different lengths and asserts
  the same high-water within a segment's slack.
- Next piece: that gauge, then bound the push's per-history structures (FileContents list, Taker brought/awaited/
  positions/manifest_of, Intaken changesets/logs) by the segment.

## 2026-11-23 ~02:40 PST (w-hg)
Landed a71609a9f:
- e21e594c9: store keepers_of reads by checked chunks (lint wall 8e5ea5489, texts.rs:576).
- e4d95bcf9: push gauges vcs.hg.push_held_high_bytes / _texts_high_bytes / push_rows_bytes. Two-history test (60 vs 240
  changesets): texts 32,768 both, rows 35,860 vs 143,780, held 89,577 vs 241,536. The admission grows ~843 B a
  changeset (bookkeeping kept to the push's end): the next piece's red ("held equal within a segment").
Waiting: arena-5 VmHWM. If the bookkeeping is next: bound it by the segment (FileContents list, awaited, brought,
positions, manifest_of, Intaken), so held is equal within a segment.

## 2026-11-23 ~05:00 PST (w-hg, arena-5 CI 45127535742: peak 2.1 GB, floor 1.73 GB mostly allocator retention, 57 GB churn)
Landed 4eeb99b67 (piece 1dc65ad07): a push's churn per changeset is one manifest copy (was 3.20 at a 91 KB manifest).
kept_as is given the text (append_revisions_after's `given`); encode_lines walks by cursors. Test:
crates/hg/tests/push_heap.rs (LargeMapped plus an arena storage like the daemon's; heap() readings' own cost taken off).
Gotchas for the next person: heap() allocates a residency buffer for every mapped block; HeapWindows puts windows on the
system heap, the daemon's arena does not.
Next: the x86 348 ms single step (wrote=43); branchmap whole 155 ms cpu (x86) later.

## 2026-11-23 ~07:30 PST (w-hg)
- The x86 348 ms step (wrote=43) was a `lookup` of a branch name: branch_tip scanned every revision's branch, then
  branch_head_revs walked the whole history (= branchmap whole, 155-161 ms on x86).
- Landed fa14530ba (main's ruling (b)): HgIndex keeps branch heads (index::BranchHeads), updated in
  append_changesets. Views serving the whole changelog answer from them; others walk (walked_branch_head_revs is the
  oracle). Tests: proptest oracle over grouped appends; lookup and branchmap on a long line read ≤ 8 (red: 512/511).
- Open: views with left-out changesets still walk; a cross-branch merge's uncertain walk is bounded only by the
  branch's lowest head.
Waiting: arena-6 (churn) and arena-7 (lookup/branchmap times) from main.

## 2026-11-23 ~08:30 PST (w-hg)
- Landed 2b6c49e6b: BranchHeads::longest_walk plus vcs.hg.branch_heads_longest_walk (a high-water at each index swap).
  Test: a 400-revision line taken across is walked in one append and counted. Slice it if the arena shows walks past
  a step's worth.
- The left-out view (secret or hidden) isn't reachable from the hg matrix: no evolution, pushes public, no import.
Waiting: arena-6 and arena-7.
- 83f1d4c00: GAPS records changeset evolution as the next completeness item: an evolution-enabled matrix row (amend
  and rebase pushed with markers), the hidden view served, its branch heads kept. It's next after arena-6 and arena-7,
  unless they name a memory or time item.

## 2026-11-23 ~10:00 PST (w-hg, arena-6 CI 45136304371: churn 45.05 GB from 57.3; peak and floor unchanged)
- Intake churn by site (throwaway sampling probe, 2,000-file manifest, 1,501 changesets): ~166 KB a changeset.
  delta::apply 84% (the one manifest copy); index key building 5%; Placed growth 1.6%; Planner 1.6%; records 0.5%.
- So the intake is ~6 GB of push-all at hg's sizes; most of the 45 GB is server-side or in the clone/stream/verify
  rows. Proposed to main: a server-level counting binary (crates/server/tests/hg_push_heap.rs) to name it, then the
  cuts by size, including the manifest buffer reuse (apply into the previous manifest's Vec, swap).
Waiting: main's order, arena-7 and arena-8 (MALLOC_ARENA_MAX=1 test of retention).

## 2026-11-23 ~11:30 PST (w-hg)
- Landed a4debd938 (merge df71a0ed5): the intake rebuilds a manifest into the previous one's buffer (Taker::spare).
  push_heap: 102,045 down to 10,766 B an edit at 1,600 files.
- Server heap binary: asked main to choose a route. (A) an integration test with an anchor/issuer/store harness and real
  hg subprocesses; (B) a lib test with a cfg(test) global allocator; (C) a sampling allocator in cli main on a diag
  branch over the real matrix. I recommended C for the numbers, then A as the lasting test.
Waiting: main's route; arena-7 and arena-8.

## 2026-11-23 ~12:30 PST (w-hg)
- (C) is ready: branch w-hg-diag/alloc-sites at e3bbd2cc8 (DIAG only, not run; main pushes it as arena-9). It adds an
  env-gated sampling allocator in cli main.rs with stamped cumulative top-site dumps, ROW-START stamps in
  hg-matrix.sh, and a line-tables build.
- Process slip: used `git checkout -b` in w-hg-a712, against the no-checkout rule; nothing was lost. Next work goes in
  the new worktree ~/slates-work/w-hg-b (agents/w-hg-b, from origin/main via `git worktree add`). w-hg-a712 is left on
  the diag branch.
- Next: (A), the server integration harness (crates/server/tests/hg_push_heap.rs): an in-process Daemon::start with
  a store owner, an issuer-minted token, create_repository, and real hg subprocesses (debugbuilddag history; push,
  clone, clone --stream, verify), skipping loudly without hg.

## 2026-11-23 ~14:00 PST (w-hg, worktree ~/slates-work/w-hg-b)
- Landed 9c8e4a95e: crates/server/tests/hg_push_heap.rs, an in-process daemon under real hg subprocesses (push, clone,
  stream, verify) with a byte sampler, top sites per row.
- First read: push 702 KB a changeset (HgExchange 21.7%, watchdog p99 14.4% plus its beat 9.5%, phase_heads 8.8%,
  ChunkWindows::put 4.6%); clone and stream about 26 KB a changeset.
- Flagged to main: the watchdog's per-beat allocation (several GB over a push-all); push::phase_heads collected per
  step; the HTTP exchange's buffering.
- Next (unless main routes otherwise): phase_heads and the HTTP exchange site.

## 2026-11-23 ~15:30 PST (w-hg, worktree w-hg-b)
- Landed dcf71036e: six R3 literals. The record versions end with a Format: line; homes' 16/20 are named; the test
  hook's 1 ms poll is replaced by a waker (release_parked).
- Landed f14ea81bb: the heap sampler no longer counts its own allocations (a thread's first symbolization was being
  charged to its next allocation); its counter is per thread. Corrected push: 362 KB a changeset, 86% Greens::lend
  (Green::default's 2 KiB SetHash plus a Box per lend). phase_heads and the watchdog were artifacts.
- Told main arena-9's diag sampler shares the guard flaw; it's minor at full scale.
- Asked main whether I take the greens lend cut or route it. Next: (c) the HgExchange site (likely also an artifact),
  then lookup's `prefixed` 91 ms begin.

## 2026-11-23 ~17:00 PST (w-hg, worktree w-hg-b)
- Landed 1af2b501d (merge 38001e7dc): a lineage member's lend allocates nothing (lazy SetHash lanes; Lent in a
  Lineage::lent slot, no Box). hg_push_heap's push went from 362,278 to 49,909 B a changeset; the test asserts lend
  bytes are 0.
- Pre-existing failure on main (w-svn's), reported to main: svn_commit
  a_revision_refused_at_publication_leaves_the_green_where_the_log_is_and_the_next_lands.
- Next: lookup's 91 ms `prefixed` begin (a node-prefix index, or the lookup's own slices).
- Landed d55c060b3: a node prefix's lookup is read by node from the index (HgIndex::changesets_prefixed), with no
  hex walk. Oracle against the walk; at most 8 reads.
- Queue clear. Next: changeset evolution (an evolution row, the hidden view, its kept heads), unless arena-10 names a
  memory item.
- Landed 8dd4adbb5 (evolution piece 1, be9ffd98f): pushed obsolescence markers are kept and served. The new
  hg_evolution test runs real hg with evolution = all, pushes an amend and a rebase, and clones: the clone holds the
  2 markers and the successors. A marker on public a is refused in 7.2's createmarkers words. The real-hg harness is
  tests/common/hg.rs. Flaky under a parallel run: t_goal93 landed-pack let-go (git; passed 3/3 alone).
- Next: kept branch heads for a view that leaves changesets out. Still owed: non-publishing repos and the hidden
  view, rewrite admission, the obsolete pushkey namespace, the "N new" output.
