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
