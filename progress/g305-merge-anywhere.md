# g305 — A-658 merges computed anywhere, committed by position (UI goal 93, 51cef9f7)

Started 2026-11-18 ~23:05 PST. Supervisor: main.
Worktree: ~/slates-work/g305-merge-anywhere (branch agents/g305-merge-anywhere, from origin/main df50dc58c;
repo = ~/slates-work/g066-fs-server/.git).

## State (2026-11-19 ~00:30 UTC; the Studio d1323898 OFFLINE since ~23:45 PST, out of memory; probed every ~5 min)
- [x] preamble, lint habits, CLAUDE.md §8, design A-645/A-642/A-635/§4.16, crates/merge, push.rs, verbs.rs read
- [x] part 1 written: red 508eabb64 (branch agents/g305-red-part1), green 435d05531 (agents/g305-merge-anywhere),
      worktree ~/slates-work/g305-merge-anywhere. Docs written with "EVIDENCE PENDING" in the A-658 part 1 entry.
- [x] part 2 written: red 5bd844b4d (agents/g305-red-part2, at_head stubbed), green code bece20c9e (agents/g305-part2),
      worktree ~/slates-work/g305-part2 (branched from part 1). Part 2 docs NOT yet written (after part 1 lands).
- [ ] Studio runs: red1 (compute_tests fail), green1 (slates-merge --lib all + fmt; slates-server --lib once, verbs.rs
      touched), merge part 1 to main; red2, green2 (slates-merge --lib), merge part 2.

## Landed
(none yet)

## Left
(all)

## Studio constraints (main, 2026-11-18 ~23:40): CARGO_BUILD_JOBS=4; one cargo command at a time; delete each red/green
target dir as soon as its result is read; a run that "did not answer" or is killed: wait a minute, rerun once, count it
neither red nor green.

## Design (part 1 = pure computation; part 2 = owner's commit by position)
- part 1: `Green::compute(&self, inc) -> Result<Computed, ComputeRefused>` (pure: fast-path counter becomes a Cell, as
  deltas_visited is); `Computed` self-contained (id, base, at, verdict); refusals BaseUnavailable / BaseAhead; the owner's
  `judge` = its conflict cache + compute; `rebase(&self)` with `Rebased::BaseAhead`; server verbs.rs map_rebase arm +
  `mut engine` dropped; tests/rebase.rs `mut rebasing` dropped. Owner answers a base past its head `BaseUnavailable`
  (was: judged as if based at the head -- a sibling finding).
- part 2: `Green::judge_computed(inc, computed)`: caches, floor, position; moved -> re-judge only the verdict units whose
  footprint the change index names in (at, head] (content units advanced from their placement at `at`, since
  map(a..c) = map(b..c) o map(a..b)); counter of units re-judged + deltas visited; oracle serial == compute on a lagging
  copy + judge_computed, census moved/re-judged/at-position.

## Resume here (when main says the Studio d1323898 is back)
1. RED1 then GREEN1 per ~/slates-work/g305-scratch/studio-commands.md (CARGO_BUILD_JOBS=4, one cargo at a time, delete
   each target after reading). Before GREEN1: in ~/slates-work/g305-merge-anywhere `git fetch origin && git merge
   origin/main`, conflict-marker grep, push the branch. Apply the Studio's rustfmt diff locally if any.
2. Fill A-658 part 1's "Tests ... EVIDENCE PENDING" line in docs/wip/SLATES_DESIGN.md with the runs (red counts, green
   counts, fmt), commit, merge origin/main again, `git diff --stat origin/main HEAD`, marker grep, then
   `git push origin HEAD:main` as its own command.
3. Part 2 in ~/slates-work/g305-part2: merge origin/main (now holding part 1), apply the drafted docs
   (~/slates-work/g305-scratch/part2-design-entry.md at the design log's end; §4.16 "Commit", A-645's bullet and GAPS'
   A-645 entry say part 2 BUILT), RED2 on agents/g305-red-part2 (`--lib compute_tests` and `--lib copy_tests`: the
   oracle, the counter and the copies' test must fail), GREEN2 on the merged branch (all slates-merge lib tests + fmt),
   fill the evidence, land the same way.
4. END: run-requests.md entry (clippy -p slates-merge -p slates-server --all-targets; xtask check: the "Shape:" consts
   HOT_VERSIONS/COLD_FILES and the shape arrays; cargo test -p slates-merge --tests (tests/rebase.rs lost a `mut`);
   cargo doc), progress, 3-line report to main.

## 2026-11-19 (jupiter, Windows x86_64; the Studio offline ~2.5 h, main moved the runs)
- part 1: red 508eabb64 compute_tests 6/6 failing (each for its own reason); merged with main 0ef8e9741 (GAPS A-645
  line + design log end resolved); green slates-merge --lib 147/147 in 78 s (baseline 141); rustfmt layout applied;
  evidence commit 0af63658a pushed (agents/g305-merge-anywhere). slates-server --lib run r1453 started (JOBS=4), main
  asked for its baseline's failures. Then: fill "SERVER RUN PENDING", merge origin/main, land.
- part 2: merged part 1 into it (2d1430f49; engine.rs conflicts: main's A-646 2b link_node -> Option, resolved in the
  hard-link unit; part 1's `held` dropped for `hold_computed`); red 5bd844b4d 3/11 failing (oracle, counter, copies);
  green 152/152 in 32 s; layout + census prints + docs 58c8ab018: 152/152 in 35 s, fmt clean; evidence c32728418
  (agents/g305-part2, not pushed yet). Census: at_position 393, moved 892 (increments 320, untouched 572), accepted 569,
  conflicts 556, before 160, flipped 262. Land after part 1: merge origin/main, diff --stat, marker grep, push.
- Not mine, on main: test-build warnings `source_of` (copy.rs:535) and `Work::symlink` (copy_tests.rs:107) never used.
- Tell main judge_computed's final signature when part 2 lands (g268's svn Moved will use it):
  `pub fn judge_computed(&mut self, inc: &Increment, computed: Computed) -> Result<Judgment, PositionRefused>`
- part 1 slates-server --lib on jupiter (0af63658a): at 32 threads the binary was ended by the daemon watchdog (exit 4,
  shards starved); at `--test-threads=4`: 595 passed, 14 failed, 3 ignored. Failing: deadlines status_capture (known on
  main), live_ship a_holder_with_no_room..., merge_origin a_holder_follows_a_higher_epochs..., merge_origin
  a_holder_keeps_a_checkpoint..., merge_service a_holder_reads_inputs_that_repeat_a_chunk..., verbs x8
  (a_rebuilt_volume_shows..., two a_replayed_write..., a_taken_over_green_whose_id..., a_takeover_keeps..., a_verb_for_a_
  volume_whose_owner_is_fenced..., a_volume_a_publication_leaves_out..., bounded_volumes_that_fill...), xshard
  a_bounded_wait_for_room.... None touches rebase; the holder ones build bases <= head. Waiting for main's baseline names.

## 2026-11-19 ~16:30Z relaunch (fresh g305)
- Both branches were already pushed: agents/g305-merge-anywhere 7cb430ff4 (part 1 + main 4644dcec3, merged by my
  predecessor, not yet run), agents/g305-part2 c32728418. No conflict markers in either worktree.
- Compared part 1's 14 server failures with main's 23 (windows-server-baseline.txt, main 6e42e7a2c; main 4644dcec3
  changed only slates-merge since): 5 are among the 23 (deadlines status_capture, live_ship no-room, merge_origin
  checkpoint, verbs taken_over_green, xshard bounded_wait); 9 are NOT: merge_origin a_holder_follows_a_higher_epochs...,
  merge_service a_holder_reads_inputs_that_repeat_a_chunk..., verbs a_rebuilt_volume_shows..., a_replayed_write_into_an_
  overlays..., a_replayed_write_to_a_clones..., a_takeover_keeps..., a_verb_for_a_volume_whose_owner_is_fenced...,
  a_volume_a_publication_leaves_out..., bounded_volumes_that_fill.... 18 of main's 23 passed in that run: the set moves.
- Code reading: part 1 reaches the server only through Green::judge (submit goes through it; push.rs:626) and
  Green::rebase (verbs.rs map_rebase), and changes only a base past the head. The 7 verbs tests touch no green; the
  merge_service one holds an inputs archive and measures a budget delta; the merge_origin one submits bases 0 and 1 to a
  replica at or past them. A hypothesis until measured.
- r31 (jupiter, 7cb430ff4): fmt merge+server, slates-merge lib, slates-server lib at 4 threads, each failure beyond the
  23 rerun alone (--exact, 1 thread). Meanwhile: merging 7cb430ff4 into part 2 locally.
- Part 2 merged with agents/g305-merge-anywhere (part 1 + main 4644dcec3): d439c5646, PUSHED (agents/g305-part2).
  Conflicts: GAPS A-645 line (main's text + part 2's two A-658 substitutions); engine.rs decide_content region. NOT a
  pick of sides: main's A-669 (decide_handed: writes handed to a link) and A-670 (an unlink/rename-over refused when a
  link made after the base names the file) read WHICH LINKS NAME A FILE, which a path footprint misses (a link's change
  is recorded at the link's name only). Port: hand-over unit per written path when the work removes/replaces anything
  (unit set stays work-only); unlink/rename footprints `and_links_naming(file)`; first unit (invalid+unfit, which A-669
  made head-dependent via handed_home) `links_naming(unlinks + rename dsts)` (Units::constant removed); Since gains
  `relinked` = each changed link's target at `at` and at head (Green::since). Copies never bring/land over links
  (copy.rs destination_refused / copy_of_source), so no copy rule. 4 tests in compute_tests.rs (a_link_made_after_...
  removal / rename_over; a_link_removed_after_... removal; ..._fit_of_the_writes_handed_to_it). Docs: part 2 entry,
  §4.16 Commit, GAPS clause; "MERGED-TREE RUN PENDING" line in part 2's entry to fill.
- Next run (after r31 ends; script ~/slates-work/g305-scratch/p2-jupiter.sh): fmt, all slates-merge lib with census,
  then red variants r1..r5 (patches in ~/slates-work/g305-scratch/red2), each `engine::compute_tests::`. Expected red:
  r1 removal tests (made + removed), r2 rename test, r3 fit test, r4 removed-link removal + fit, r5 made-link removal +
  rename.
- judge_computed's signature unchanged: `pub fn judge_computed(&mut self, inc: &Increment, computed: Computed) ->
  Result<Judgment, PositionRefused>` (tell main for g268 when part 2 lands).

## 2026-11-19 ~17:15Z relaunch 2 (fresh g305; main e3234e618)
- Part 1 merged with main e3234e618: 6fc4cedcc (pushed agents/g305-merge-anywhere). Conflicts: engine.rs Green fields
  (part 1's fast_path_hits Cell + main 71f05f93d's mapped_files_made Cell, both kept) and judge (main added
  Judged.post_state; part 1 routes judge through compute/held, so held(inc, computed) fills post_state from the
  increment, as part 2's hold_computed(inc, computed) pairs them). Computed doc: a mapped file is ops, never bytes.
- r90 (jupiter, 6fc4cedcc, script ~/slates-work/g305-scratch/p1-jupiter.sh): fmt merge+server, slates-merge lib,
  slates-server lib at 4 threads, the 9 (+ any new beyond main's 23) alone, and each that fails alone, alone on main's
  tree (HEAD^2 overlaid). Target ~/.cache/g305-target (kept), clone deleted.
- Next while r90 runs: merge 6fc4cedcc into part 2 (71f05f93d's deferral against part 2's units/at_head).

## 2026-11-19 ~19:30Z relaunch 3 (fresh g305; Studio d1323898 back; jupiter no longer used; Ada: focused unit AND integration tests)
- Committed the predecessor's pending design line (d356b3e20: a Computed holds a mapped file's ops after 71f05f93d).
- Merged origin/main 0c7a77bd3 (8bb2bc3ec) and 5c0a522f1 (e7e982d65, A-672 copy_tests only): conflicts GAPS A-645 line
  (main's A-666 part 3 + blame wording, mine "committed by position (part 1 BUILT...)") and the log's end (main's
  first, A-658 part 1 last). Tree vs main = the piece's 7 files alone. PUSHED agents/g305-merge-anywhere e7e982d65.
- No slates-server LIB test reaches map_rebase (swept every cfg(test) region); the server's rebase tests are integration.
- Studio run (script ~/slates-work/g305-scratch/studio-p1.sh): fmt merge+server; slates-merge --lib; --test rebase,
  engine whole; checkpoint `restored`; special `an_ipc_creation_rebases`; slates-server --lib verbs:: (4 threads);
  server --test daemon merge_rebase_scenario; merge_load (2); verdict_pages (2); works_off_owner (4 rebase + conflict
  re-answered); green_post_state `rebased` (3).
- Landing plan: on green, fill evidence on the branch, then a fresh worktree ~/slates-work/g305-p1-land from
  origin/main, `git merge --squash` the branch, ONE commit with a real message, diff --stat, markers, push HEAD:main.
- r23 (Studio, e7e982d65): fmt clean; slates-merge lib 186/186; --test engine 47/49: symlink_merges (MINE: its last
  work .at(5, 4) named base 4 over head 3, passed only because a past-head base was judged at the head; fixed to base 3
  + asserts v3, e54e1f558, design sibling bullet + Applied list) and hard_link_merges (link l->other at base 2 after an
  identical relink at v3: DeleteModify, expected CreateCreate; likely main's A-670 rule, to measure on main's tree);
  --test rebase never ran (cargo stopped at engine); checkpoint `restored` 2/2; special 1/1; server lib did not
  compile (main's PackRefused::Stalled, fixed at 71a43cd97); daemon filter matched 0 (the scenario runs inside
  the_daemon_serves_the_lifecycle_verbs_exactly_once_with_leases_and_typed_refusals); merge_load 2/2, verdict_pages
  2/2, works_off_owner 5/5, green_post_state 3/3.
- Merged main b1926d201 (ade7b1614, PUSHED). r55 (Studio): A) main's tree, own target: engine hard_link_merges +
  symlink_merges; B) branch: fmt, merge lib, 11 merge integration targets that submit (no-fail-fast), server lib
  push/merge_origin/merge_service, server rebase integration tests (daemon filter corrected).
- r55 (Studio): main a6306963e's own tree fails hard_link_merges identically (main's; fixed on main 8d7209388);
  symlink_merges passes there (mine, fixed). Branch ade7b1614: rebase.rs proptest
  a_rebases_changes_are_the_whole_restatements_difference failed on a fresh seed (cc c249aab0...: origin empty file a +
  symlink d/y; v1 link b->a; work removes a at base=head 1); everything else green.
- r89 (Studio): fixed-input probe on main 787f5948b AND branch b77c7802d print the same: rebase Rebased{changed {a:
  None}, journal [Unlink a]} vs submit leaving {b: []} => MAIN's bug (rebase restatement misses the node handed to a
  link when its home is removed; A-669/A-670 area). Branch b77c7802d (main 1b480bd70) ALL GREEN: fmt; merge lib
  191/191; merge integration 104 passed 1 ignored; server lib 42/42 (push, hg_push, merge_origin, merge_service);
  server rebase integration 13/13. Evidence in design (252e73750).
- Main moved to af520142f touching repository/push.rs + verbs.rs: merged (3b5fc4874, pushed), r113 = same focused
  set on that tree. Then: squash-land from a fresh worktree off origin/main (one commit), push HEAD:main.
- r113 (Studio, 3b5fc4874 = branch + main af520142f): alike r89 (merge lib 191/191, server lib 42/42, server rebase
  integration 13/13, fmt clean); rebase.rs oracle met main's removed-home bug again (seed cc 17ddd448..., origin empty a
  + link b->a, work removes a at version 0).

## LANDED 2026-11-19 ~20:05Z: part 1 = main 545181b30 (one squashed commit over 3329211e0; worktree REMOVED after landing,
   ~/slates-work/g305-p1-land, branch agents/g305-p1-land local only). The tested branch agents/g305-merge-anywhere
   (18cbbf785, pushed) holds the history (red 508eabb64 included). Studio target ~/.cache/g305-target and logs DELETED.
   Run requests filed (run-requests.md, 2026-11-19T20:10Z g305). Reported to main: landed + findings (main's rebase
   oracle bug; the nine Windows server failures; hard_link_merges was main's, fixed 8d7209388).

## Left
- Part 2 (when main starts it): ~/slates-work/g305-part2, branch agents/g305-part2 (d439c5646 pushed; later local work
  per the notes above). Merge origin/main (now holding part 1 as the squash 545181b30: identical hunks should merge
  clean; the design log entry and GAPS A-645 line may conflict) and 6fc4cedcc's 71f05f93d deferral against part 2's
  units/at_head; then focused tests on the Studio (lib compute_tests/copy_tests + merge integration + server as part 1),
  and land as one commit the same way. judge_computed's signature unchanged:
  `pub fn judge_computed(&mut self, inc: &Increment, computed: Computed) -> Result<Judgment, PositionRefused>`.

# PIECE 2026-11-19 ~20:15Z: A-677 (main's assignment; outranks part 2) — the rebase's restatement omits the node a
# removal hands to a hard link (rebase.rs oracle; seeds cc c249aab0..., cc 17ddd448...)
- Claimed in bug-claims.md; A-677 taken in amendments.tsv (backup amendments.tsv.bak-g305-a677).
- Worktree ~/slates-work/g305-rebase-handed, branch agents/g305-rebase-handed from main 545181b30.
- Plan: 1) red: the probe as a unit/integration test (+ both seeds in a new rebase.proptest-regressions, the oracle
  made `seeded`); 2) fix rebased_from to carry the handed node to the heir link as the commit does (A-669); 3) Studio:
  merge lib, --test rebase / --test engine filtered, server rebase integration by name, fmt; 4) land one commit.
- Red 05593caea (agents/g305-a677-red): node_tests unit test + rebase.rs pinned cases + seeded property (new
  rebase.proptest-regressions with both seeds). Fix 79ec79e77: Green::heirs_of (node_moves' SetContent heirs) folded
  into rebased_from's changed after the work's own; journal unchanged. Docs 48589035e (design entry A-677, §4.16
  status, docs/bugs/2026-11-19-a-rebases-files-lost-the-link-a-removed-home-handed-its-node-to.md).
- r156 (Studio): RED node_tests 24 passed 1 failed (the new case); rebase 1 passed 2 failed (named cases + property at
  first replayed seed). GREEN 79ec79e77: fmt clean, merge lib 192/192, --test rebase 3/3, engine rebase 4/4, special
  rebases 1/1, server rebase integration 13/13.
- Merged main 1ef718820 (A-676 touched engine.rs/node_tests.rs; server verbs + rebase test files changed): 878a2eb43,
  pushed; r172 = the green set again on the merged tree. Then squash-land (fresh worktree off origin/main).
- r172 (Studio, 878a2eb43 = branch + main 1ef718820): fmt clean; merge lib 193/193; --test rebase 3/3; engine rebase
  4/4; special rebases 1/1; server rebase integration 13/13.

## LANDED 2026-11-19 ~20:35Z: A-677 = main 031e04337 (piece 9726e11c3 squashed over b04c552b0; merges bfcf40e13 and
   031e04337 after a refused push: main's A-675, --vcs-http, A-601 touched none of the rebase paths). bug-claims and
   amendments.tsv marked landed. Worktrees g305-rebase-handed and g305-a677-land REMOVED (branches pushed:
   agents/g305-rebase-handed, agents/g305-a677-red). Studio target and logs DELETED. Run requests filed
   (2026-11-19T20:40Z g305). Reported to main.

## Left
- Part 2 of A-658 (when main starts it), unchanged from above: ~/slates-work/g305-part2.

# PIECE 2026-11-19 ~20:45Z (main's assignment; outranks part 2): A-677's siblings -- does a rebased work show a heir
# link's inherited mode, attributes and handed-on FIFO as the submit keeps them? One test per kind; a fix if any
# differs (claim + next A-number), else the reason recorded in A-677's entry.
- Worktree ~/slates-work/g305-heir-meta, branch agents/g305-heir-meta from main.
- Server view: a work's verbs are ReadRange/ReadWindow (bytes), ReadDir (files+sizes, declared dirs/symlinks;
  listing::work_paths), Status: no mode, attribute or IPC node, so a rebased work holds those in its journal alone.
- Tests (node_tests.rs): mode and attributes: a rebased work's next submit (restated journal sealed+submitted) vs the
  work's own submit -> equal (pass on main's code). FIFO: deriver refuses rebased or not: removal Unsupported both;
  rename PathKindConflict as declared, Unsupported as restated (r225 showed the kinds differ) -> pinned (tripwire).
- r231 (29a35798d): fmt clean, node_tests 29/29, lib 197/197, --test rebase 3/3.

## LANDED 2026-11-19 ~20:52Z: A-677's siblings = main bb535bd4d (piece efcafcc8a squashed over 34f655916 + merge of
   1561487f0). No bug, no A-number: the reason recorded in A-677's entry, the bug record and §4.16. Worktrees removed
   (branch agents/g305-heir-meta pushed); Studio cleaned. Run requests filed (2026-11-19T20:55Z). Finding for main:
   GAPS' A-643 siblings item 1 stale (a rename over a linked IPC node is accepted by the verdict now).

## Left
- A-658 part 2 (when main starts it): ~/slates-work/g305-part2.

# PIECE 2026-11-19 ~21:00Z (main's assignment after the restart): A-658 part 2 -- the owner commits a computed verdict
# by position (~/slates-work/g305-part2, branch agents/g305-part2 at 760418b6b, pushed).
- Merging origin/main bb535bd4d: conflicts engine.rs (12), compute_tests.rs (5, add/add), lib.rs, tests/rebase.rs,
  GAPS, SLATES_DESIGN (7). Red variants r1..r5 in ~/slates-work/g305-scratch/red2 (one per A-669/A-670 guard).
- Merged main bb535bd4d (1ecfdce75) and ee5cbed4c (f1f3e7532, PUSHED). engine.rs/design doc merged 3-way with part 1's
  state 6fc4cedcc as the base: engine.rs 0 conflicts; design doc 1 (part 2's entry appended after main's). lib.rs keeps
  PositionRefused; compute_tests.rs = part 2's (main's == part 1's); rebase.rs = main's (A-677). Diff vs main = part 2's
  7 files, same stats as part 2 over part 1. A-676: hard-link unit footprint pair(link, target) covers node_home's
  reads; A-677: heirs derived at the commit's head (node_moves), rebase via decide (units wrapper) -- consistent.
- judge_computed signature UNCHANGED: `pub fn judge_computed(&mut self, inc: &Increment, computed: Computed) ->
  Result<Judgment, PositionRefused>`.
- r34 (Studio, after kiln's restart): green set as part 1's landing + red r1..r5 (compute_tests::) in a copy.

## 2026-11-19 ~21:25Z relaunch 4 (fresh g305; brief: merge origin/main, check the A-669/A-670 port against A-677's
## heirs, red five variants then green focused, land one commit + merge, END)
- r34's logs (Studio ~/.cache/g305-logs/20261119T210131Z, tree f1f3e7532): fmt 1 (two reflows in engine.rs); merge lib
  202/206: the 4 new port tests failed at compute_tests.rs:550 ("not accepted: Conflict TypeChanged at h"), i.e. the
  setup work making link h was refused before any guard ran, so r34's red r1..r5 (each "4 failed") prove nothing.
  merge integration 0, server lib 0, daemon 0, merge_load 0, verdict_pages 0, works_off_owner 0, green_post_state 0.
- Predecessor's uncommitted fix in the worktree: rustfmt's two reflows + Draft::naming interns a NodeRef (origin Base)
  for a Link (A-646: a link names a node-table entry). Next: verify, commit, merge main 3612d1ef9 (A-626 part 4).
- Committed the predecessor's pending fix (51a1db1c0). Main's part 1 compute_tests.rs still has the old helper: its
  oracle has generated no accepted link since A-646 part 2b (33a705d8c) -- finding for main (part 2 fixes it).
- Merged main 3612d1ef9 (A-626 part 4: 32932432f; conflicts copy_tests imports = Judgment kept, Rebased dropped; design
  log end = main's two entries first, part 2 last, exact union) and b5231f0ff (d0d2a5231, docs + a vcs_http comment).
  PUSHED agents/g305-part2 d0d2a5231; over main = part 2's 7 files alone.
- Port vs A-677 checked by reading: commit derives node moves at its own head (commit -> make_mapped -> node_moves), a
  Computed never holds them; heirs_of runs only in rebased_from at the computing green's head (rebase = decide at head,
  never by position). merge_handed reads heir's link change, home's change/bytes/mode/xattrs only: pair(heir, home) ok.
- Expected red: r1 -> made-removal + removed-removal; r2 -> rename; r3 -> fit; r4 -> removed-removal + fit;
  r5 -> made-removal + rename. r4/r5 lines re-targeted at rustfmt's reflow ("self.hardlink_at(path, at)," / "self.
  hardlinks.get(path).cloned()," -> "None,").
- r91 (Studio, d0d2a5231; script ~/slates-work/g305-scratch/studio-p2-run2.sh): red r1..r5 then green (fmt, merge lib,
  11 merge integration targets, server lib 4 filters, server integration as r34).
- r91 (Studio, d0d2a5231): RED as predicted (r1 3 failed: made-removal, removed-removal, oracle; r2 2: rename, oracle;
  r3 1: fit; r4 2: removed-removal, fit; r5 3: made-removal, rename, oracle); GREEN fmt clean, merge lib 210/210 (by-
  position census at_position 410, moved 1058, increments 391, untouched 667, accepted 713, conflicts 516, before 239,
  flipped 352), merge integration 105 passed 1 ignored, server lib 48/48, server integration 13/13.
- Main moved to 0577ab0bd (A-654 part 2: git push driver -> push_sealed -> Green::judge): merged (f7219c7f9, pushed);
  r115 (Studio): fmt merge+server clean, server lib + git_push:: git_protect:: 54/54. Evidence commit 49026464c.

## LANDED 2026-11-19 ~22:00Z: A-658 part 2 = main 092386776 (piece bc5f2a0d1 squashed over 34d10acb5 + merge of
   9d54e0f9a after main moved; db/svn/ssh-test only). Branch agents/g305-part2 (49026464c) pushed with the history.
   Worktrees g305-p2-land, g305-part2, g305-merge-anywhere REMOVED (all clean, branches pushed). Studio target
   ~/.cache/g305-target and logs DELETED. amendments.tsv A-658 row marked landed (backup .bak-g305-a658p2; g302's A-683
   written concurrently is intact). Run request filed (2026-11-19T22:05Z). judge_computed's signature UNCHANGED:
   `pub fn judge_computed(&mut self, inc: &Increment, computed: Computed) -> Result<Judgment, PositionRefused>`.

## Left (owed, main's to assign)
- The fleet's route (a holder computes, the owner takes by position); the server's submit and push taking computed
  verdicts; a rename's edits placed as a content edit's; flat unit storage if measured hot; A-645's hot-green run.

# PIECE 2026-11-19 ~22:10Z (main's assignment): A-685 -- `slates repository create <name> --tool git|hg|svn` (CLI +
# client verb to the control channel's CreateRepository; refusals typed to exit codes); failing tests first (the_grammar,
# a real-anchor CLI test: create, the name again taken, a --vcs-http edge with an enrolled token finds it, unknown 404).

## 2026-11-19 ~22:55Z relaunch 5 (fresh g305; A-685 then A-658 part 3; END when both land or context short)
- Predecessor's worktree ~/slates-work/g305-repo-create (branch agents/g305-repo-create) was clean at main f05af5288:
  no A-685 work. Fast-forwarded to origin/main b94ae6e90.
- BUILT, branch commit dbe8ba1ef (PUSHED agents/g305-repo-create): `slates repository create NAME --tool git|hg|svn
  [--format F] [--default-branch B] [--root GREEN] [--json]`. Server: repository::{object_formats, root_binding,
  RootRefused} re-exported at slates_server's root (git SHA-1|SHA-256 on `main`, held to git_push::ref_of under
  refs/heads/; hg SHA-1 on `branch:<B>:0`, B=default; svn revisions on `root`, no branch taken); git_push HEADS/ref_of
  pub(crate). Client: RepositorySpec + Client::create_repository; VcsTool/ObjectFormat re-exported. CLI: Verb::
  RepositoryCreate{spec}, flags --tool --format --default-branch --root, emit_repository_created (repository:/id:, JSON
  {repository,id}); tests the_grammar_parses_the_repository_verb, the_grammar_refuses_a_repository_its_tool_cannot_have,
  cli.rs a_repository_the_cli_creates_is_found_by_name_on_the_vcs_http_edge (+ start_anchor_with_vcs_http,
  bearer_request/answer_head refactor of bare_request). Docs: §4.17.1 status, A-685 entry (Runs: EVIDENCE PENDING),
  GAPS A-685 paragraph after A-575's, docs/cli.md.
- r294 (Studio, dbe8ba1ef): red (dispatch removed: grammar + cli test must fail), then fmt, green grammar, green cli
  `vcs_http` filter (3 tests: the two A-625 ones + mine). Target ~/.cache/g305-target, clone ~/.cache/g305-a685-clone,
  logs ~/.cache/g305-logs/a685.
- Landing plan: fill Runs, fresh worktree ~/slates-work/g305-a685-land from origin/main, `git merge --squash` the
  branch, ONE commit with a real message, diff --stat, markers, push HEAD:main.
- Findings for main (by reading, not chased): the server records a repository whose format its tool does not name
  (git in revisions gets no store: vcs_store.unmade counted; svn/hg ignore format); recorded in GAPS + A-685 Owed.

## LANDED 2026-11-19 ~23:10Z: A-685 = main 2bfec1547 (piece c3f8ac6fb squashed over 550153532 + merges d1be9ae32 of
   693bc8feb, 2bfec1547 of 2f73eb075). Runs: kiln r294 (red 2+1 failed / green 13 + 3), r329 on the landing tree
   d1be9ae32 (fmt clean, grammar 13/13, cli vcs_http 3 + 1 ignored). amendments.tsv A-685 marked landed (backup
   .bak-g305-a685). Run requests filed (2026-11-19T23:12Z). Worktrees g305-repo-create and g305-a685-land REMOVED
   (branches agents/g305-repo-create 3ff3423a1 and agents/g305-a685-land pushed). Studio clone + logs deleted; target
   ~/.cache/g305-target (6.7G) KEPT for A-658 part 3.

# PIECE 2026-11-19 ~23:15Z: A-658 part 3 -- the fleet route (a holder computes, the owner takes by position) and the
# server's submit and push taking computed verdicts.
- Worktree ~/slates-work/g305-fleet-route, branch agents/g305-fleet-route from main 2bfec1547.
- PLAN (decided from the code, 23:20Z). Part 3 splits into three landings:
  3a. A computed verdict's wire form (slates-merge, pure): a holder's `Computed` crosses to the owner as id, base, at,
      floor, and Reached: Before(v) | Verdict{ per unit: its effects as chain_entry `Applied` records (reusing
      put_applied/take_applied with `Declared::of(inc)`, hostile-tested already), its windows (path, range, class
      tag), its placements (`Placed`: range and point, each `map::Mapped` Shifted(range)|Overlaps) }, magic+version
      header, blake3 digest trailer as chain entries carry. Decoding is the owner's: `Green::computed_from(&self,
      inc, bytes) -> Result<Computed, DocDecodeError>` rebuilds each unit's effects with `effect_of` over the
      increment's post-state and `copy::EntryCopies::of(all units' applied)` -- the replica replay's own path
      (apply_recorded), whose replay invariant makes effect_of(applied(e)) commit as e. `applied` re-derived by
      `Reached::of`. Module: crates/merge/src/engine/computed_wire.rs (child of engine: sees Effect, Reached,
      position::Unit/Placed). chain_entry's put_applied/take_applied/Declared -> pub(crate); Placed gets a pub(super)
      constructor/accessors.
      Tests: compute_tests' oracle takes the owner's verdict through decode(encode(c)) (serial equality must hold) and
      checks encode(decode(b)) == b; hostile: every truncation refused typed, every flipped bit refused (digest),
      foreign magic/version, a count past the bytes left refused before allocating; a golden vector of a small
      Computed. Runs: slates-merge --lib engine::compute_tests:: engine::computed_wire + fmt.
  3b. The fleet route for a submit: where the submitting node holds a replica (merge_origin's replica engine), its
      shard computes on the replica and the request to the owner carries the encoded Computed (an appended wire
      variant/field; publish_appended_variants); the owner's submit (verbs.rs, today `engine.submit_resolved(&inc)`)
      takes it by `judge_computed` + `commit_judged`, falling to its own judge only on a typed PositionRefused.
      Test: a two-node f=1 server test with the client on the holder (works_off_owner-style), counter of verdicts
      taken by position.
  3c. The push (repository/push.rs: `engine.judge(work)` then commit_judged) taking computed verdicts the same way.

## LANDED 2026-11-19 ~23:50Z: A-658 part 3a = main fc5f4ea98 (piece 0ba1006c4 squashed over f7684fc20 + merges of
   5206419be, 2c3fa2bcc+075e36b52 (7033cf210), 9ca6e3883 (fc5f4ea98); each merge checked as the exact union against
   the commit merged: over it the piece's 8 files, +648 -9). Runs r391 (branch) and r431 (landing tree 81d07e3): fmt
   clean, slates-merge --lib computed_wire_tests + compute_tests:: + chain_entry:: 22/22. amendments.tsv A-658 row
   updated (backup .bak-g305-a658p3a). Run requests filed (23:55Z). Worktrees g305-fleet-route and g305-p3a-land
   REMOVED (branches agents/g305-fleet-route 1c5a31196, agents/g305-p3a-land pushed). Studio clones, logs and target
   ~/.cache/g305-target DELETED.

## Left (A-658 part 3, for the next g305; plan above, "PLAN (decided from the code, 23:20Z)")
- 3b: the fleet route for a submit. Facts found: a holder's replicas are `state.merge.replicas: BTreeMap<ObjectId,
  Green>` (crates/server/src/merge_service.rs:83) on the shard holding the green's register object; the owner's submit
  is `engine.submit_resolved(&inc)` in crates/server/src/verbs.rs (~line 9491, inside the submit's admission and
  retention charges); cross-node verbs go through `verbs::forward_to_owner` (verbs.rs ~1315). Shape: where the
  submitting node holds the green's replica at or near the work's base, compute there (`replica.compute(&inc)`), send
  `Computed::encode(&inc)` with the forwarded submit (an appended wire field or variant, published with
  `cargo test -p slates-ipc --test wire_order -- --ignored publish_appended_variants`); the owner reads it back
  (`computed_from`), takes it (`judge_computed` then `commit_judged`), and judges itself only on a typed
  PositionRefused or a read refusal, counted. Test: a two-node f=1 server test with the client on the holder, a
  counter of verdicts taken by position (non-vacuity), outcomes equal to the owner judging alone.
- 3c: repository/push.rs (`engine.judge(work)` then `commit_judged`) and hg_push taking computed verdicts the same way.
- Findings sent to main (A-685): server records a repository whose format its tool does not name; g083's hg interop
  #[ignore] reasons still name A-685.

## 2026-11-20 ~00:20Z relaunch 6 (fresh g305; queue: 3b submit's fleet route, then 3c pushes)
- Read relaunch-2026-11-19.md (completeness first), restart preamble, lint habits, CLAUDE.md §8 (origin/main).
- main has 3a (fc5f4ea98) and nothing of 3b/3c. Worktree ~/slates-work/g305-fleet-submit (branch
  agents/g305-fleet-submit from origin/main 574185afa), nothing written yet.
- BLOCKED ON THE PREMISE (found by reading, evidence below): in a fleet a submit's increment exists only on the
  green's owner node, so no holder ever has an increment to compute before the verdict, and "the forwarded submit"
  from a holder node carries only `Submit { work, evidence }`.
  - A client's `CreateWork { green }` on node B for a green A created is forwarded to A (verbs.rs
    `owned_by_another_host`: creator != host and B's partition holds no record of the green), and A-247's rule says
    "A creation forwarded from another node is served on the green's owner, which places it there" (design ~21869).
  - So every later verb of the work (its id's creator host is A) is forwarded whole to A (`serve_here` ->
    `forward_to_owner`); the work's journal and files, and the increment composed from them (work_home `composed`,
    or the owner's `build_increment`), live on A only.
  - Works across nodes are A-247 step two, open in GAPS ("Open (perf, 2026-11-08): A-247's step two, works across
    nodes": owes the work's node rule, the owner's asks over the fleet exchange, the lease gate on a remote verdict).
  - Shard replicas within a node were removed (A-528), so no shard but the green's own holds its versions on a node.
  - 3c has the same premise: a git/hg push is served on the repository's owner shard on the node it connects to
    (vcs_http daemon_edge routes by owner_of_name within the node).
- Routes: (1) A-247 step two for holder nodes: a work made on a node holding its green's replica lives there; its
  base reads and rebase answered by the replica, its increment composed there and its verdict computed on the
  replica, only the commit by position asked of the owner (the forwarded verdict ask carries increment + form): no
  added round trip; several pieces (creation/rights/naming, floors at the owner, the verdict ask over the fleet
  exchange, settle once the replica holds the version, recovery, takeover). (2) The owner hands the increment to a
  holder to compute and takes the form back by position: one piece, but +1 round trip and a second computation a
  fleet submit; a throughput-vs-latency trade only main's A/B can settle.
- Reported to main; waiting for its decision (no code written).

# PIECE 2026-11-20 ~03:55Z relaunch 7 (fresh g305; brief briefs/g305.md): A-697 (A-247 step two for holder nodes)
- Predecessors r3-r5 (16:04-19:50 PST) left NOTHING of A-697: no branch, no commit, no notes; worktree
  ~/slates-work/g305-fleet-submit (branch agents/g305-fleet-submit) clean, fast-forwarded to origin/main b63f8a05d.
- Plan accepted by main (focus-84-90.md): (a)-(f) land with production routing unchanged, each tested through a
  TEST-ONLY daemon hook gated like inject_merge_fault; the LAST piece deletes the hook, adds the real "replicated here"
  rule + the two-node f=1 e2e test with the taken-by-position counter. Then A-658 3c.
- Now: piece (a) -- the owner admits a holder's work by a forwarded verb (rights from the owner's record, base held),
  the holder's replica answers base reads, the work made on the client's partition; test two-node f=1, client on B,
  hook on: work id names B, G's files byte-identical, edit reads back, no-rights principal refused by A, same name
  AlreadyExists; amend A-247's rule (design + amendment-log entry).
