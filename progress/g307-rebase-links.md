# g307 — A-663 (rebase restates links by node), then A-626 parts 3 and 4 (UI goal 93, kiln 51cef9f7)

Started 2026-11-19 ~09:00 UTC. Supervisor: main. Implementer (THE RULE, §8): unit tests + fmt only, on the Mac Studio
d1323898 (adas-mac-studio.local-2), CARGO_BUILD_JOBS=4, one cargo at a time, target dirs deleted after reading.
Repo for worktrees: ~/slates-work/g066-fs-server/.git. Scratch: ~/.local/state/slates-fanout/g307/.
Studio OFFLINE from ~09:20 UTC (main's notice); checked every ~5 min with `kiln run true` (on adas-mac-studio.local-2).

## Relaunch 2026-11-19 ~10:15 UTC (fresh g307): builds on jupiter (Windows x86_64), the Studio offline
- Jupiter recipe: ~/.local/state/slates-fanout/windows-build-recipe.txt; clone with
  `git -c http.version=HTTP/1.1 -c http.sslBackend=openssl -c core.autocrlf=false clone ...` (main, 11:30 UTC: the
  plain clone died with a TLS reset once). slates-merge's lib test build took 36m53s (red, loaded box) and 10m12s (green).
- A-663 LANDED 4b12a582e (one commit over 556a4e050; worktree ~/slates-work/g307-a663-main, branch
  agents/g307-a663-main). Scratch: agents/g307-a663-land (97231630c, a2b6d62bd: the tested tree, origin/main 78a670256 +
  A-663). Red 6/6 at 91d14b44d (jupiter 10:26 UTC); green slates-merge lib 147/147 (main's 141 + 6) at a2b6d62bd, fmt
  clean; census of 256 works: agreed 153, refused 102, conflicts 1; replaced 5, renamed_then_linked 30,
  linked_then_renamed 32, removed 13, made_again 26, to_made 8. macOS run filed owed (run-requests.md).
- The red oracle's minimal case [(31,13,0),(22,88,0)] (append g; rm g, h links g) was a LOST WRITE outside A-663:
  the deriver drops a removed file's content ops (apply_unlink clears them) and Green::hand_over gives the heir link
  the head's bytes. Filed open in GAPS + bug-claims (unclaimed, first in line); the oracle's works leave it out
  (Draft::strands_an_edit). The draft's sibling "`ln g l; mv f l` composes to both" was wrong (refused
  PathKindConflict by check_no_file_directory_collision): dropped before landing.
- lint-habits.md's marker grep (BRE with `\|`) never matches `=======` on macOS: use
  `command grep -rnE '^(<<<<<<< |=======$|>>>>>>> )'` (told main).
- Main 11:30 UTC: slates-anchor's Windows baseline at 018d32ba2 is 31/31; run A-626 part 3 on jupiter after A-663 lands
  (slates-merge + slates-anchor). Part 4 also needs slates-server: its baseline was running (r1401); wait for main's
  number before running part 4's server tests.

- Main 12:15 UTC, CHANGE OF ORDER: the lost update first, registered and claimed for me as A-669 (a lost write
  outranks a feature). Failing tests first (base g + link h: append g then rm g; append g then mv f g; each keeps the
  write or is refused typed); the rule from §4.16/A-646 (the heir inherits the node as the work left it, not the
  head's) written into A-646's entry; fix apply_unlink and hand_over; the oracle stops steering around it
  (strands_an_edit removed) and its census counts it; sweep siblings (any heir handover, rename-over, replace, ops
  cleared before a handover). Land alone on jupiter (slates-merge) with a docs/bugs note. THEN A-626 part 3.

- A-669 LANDED on main 6e42e7a2c (85b5b7eee code+tests+docs; e5aac9bf6 merged-tree evidence; merge). Red 6/11 at
  59ee404f5 (five cases + oracle `create l; mv g f; append f; rm f`); green 1 at ae27b01d3 found `ln h l; rm h` (a link
  resolved at the seal) -> linked_nodes; green 152/152 at 72177dcd7 and 5f6cc2d3a over 0e755c9b1; merged 169/169 at
  85b5b7eee over f6ec36df2 (A-659 stacks); fmt clean; Windows x86_64 jupiter; macOS owed (run-requests). Census:
  handed_removed/replaced shown, not required (2/1, 5/0, 4/1). Filed: concurrency sibling (a later version's link,
  GAPS open, bug-claims), a question on A-659's stack judge with hand-overs. Scratch branches: agents/g307-a669,
  agents/g307-a669-red, agents/g307-a669-land (all pushed). The Mac disk filled twice (ENOSPC); I removed my pushed
  worktrees g307-a663*, g307-a669-red, g307-a626p3, g307-a626p4 (all clean, recreate from their pushed branches).
- NEXT: A-626 part 3 (main 12:15 UTC: after A-669), on jupiter: slates-merge + slates-anchor (anchor baseline 31/31 at
  018d32ba2). Recreate the worktree from agents/g307-a626p3 (acd417576), merge origin/main (A-663, A-669, A-659 landed
  since; engine.rs conflicts likely), red/green as the brief says. Part 4 waits for main's slates-server baseline.

- Main ~13:55 UTC, ORDER CHANGED AGAIN: A-670 (claimed for me) before A-626 part 3. (1) the concurrency sibling: failing
  tests first, fix under A-669's rule (write with its node, or refused typed), oracle widened to generate it; (2) A-663's
  census: guarantee every counted case (fixed histories or a seeded floor), never vacuous, never luck; (3) A-659's stack
  judge with an upper handing a write to a link: answer with a test, fix if needed. Land alone on jupiter, then part 3.
  Part 3 PARKED at agents/g307-a626p3-land f87d52b0f (pushed): part 3 cherry-picked over main 6e42e7a2c, the design
  conflict resolved (main's entries then part 3's); untested; its worktree removed.

- A-670 LANDED on main 4644dcec3 (one commit over 6e42e7a2c). Verdict: removing a file / renaming over it refused when a
  link naming it was made after the base and no base link outlives the work (DeleteModify/RenameRename), unless handed
  (A-669); stack judge: upper_unseen_links (Against::since = stack base + landed versions); A-659's question answered by
  test (stack carries a handed write; nothing to fix); restate oracle: heads Plain/LinksFile/LinksTarget (LATE = "k"),
  fixed_histories meet every census case each run; A-643's whole-state model states the rule (targets_at per version).
  Red at cac45de71: 6 failing/14 passing; green 177/178 at ac6a83b09 (A-643's model), 178/178 at e8c10097e; Windows
  x86_64 jupiter; fmt clean; macOS owed (run-requests). Worktrees removed; branches agents/g307-a670{,-red,-land} pushed.
- NEXT: A-626 part 3, parked at agents/g307-a626p3-land f87d52b0f (part 3 over main 6e42e7a2c, design conflict
  resolved, untested). Main now 4644dcec3 (A-670): merge it in (engine.rs may conflict: decide_content/decide_moves vs
  part 3's restore_* refactor), then slates-merge + slates-anchor lib tests on jupiter, single landing commit.

- A-626 part 3 RUN on jupiter (2026-11-19 ~14:30 UTC) over main 4644dcec3: branch agents/g307-a626p3-final 7d08dd16a
  (part 3 cherry-picked over main; worktree ~/slates-work/g307-a626p3-final, clean). slates-anchor lib 31/31 (= main's
  baseline), fmt clean (merge + anchor); slates-merge lib 180/181: copy_tests::every_copy_reads_as_its_model_over_every_
  history FAILS at copy_tests.rs:1992, `restored.head_identity() == restored.head_identity_from_scratch()` (the running
  identity matched the original at every version; only the recount differs). Minimal-ish input (shrinking stopped at
  512): [Put, Mkdir, Put, Copy{back 215, replace: true}] -> a copy made after the checkpoint from a source version
  before it. Cause (by reading): copied_tree_entries reads the source at its source version through the restored
  green's histories, which start at the checkpoint (deltas_from); head_identity_from_scratch filters copies by
  record.version > deltas_from, not by source_version. Exposed too, by reading, unverified: the running identity's
  copied_entries (a copied key changed later gives back the entry read at source_version through histories) -> a
  restored replica's identity could diverge. Replay itself is right (the recorded fix). Proposed fix (part of v4, not
  landed yet): at copy commit, when source_version < the history floor, record the entries the copy gave each key (and
  its correction for the recount) in the copy record, carried by the checkpoint's CopyRecord; copied_entries and the
  recount read those instead of pre-floor histories; failing oracle seed cc ef9d3557... kept as the red. Waiting for
  main's word on scope (reported 14:45 UTC).

- Main ~15:00 UTC on part 3's defect: do NOT record copied entries (O(tree)); fix by (1) no pre-floor read (map's
  current value / the copy's grafted node) or (2) a floor rule; failing test for replica divergence; keep seed
  cc ef9d3557 as a regression; minimal v4 change; then land part 3.
- CHOSEN (1), by the graft: brought_entry reads the source at V-1 through the copy's fix (kept in Appearance.fix,
  charged with the change index via record_bytes, reserved via changes_reservation += copy_entry_room, released by
  release_kept_fixes in fold_history_before when floor >= V; restored records carry none). (2) rejected: failing copies
  are made after the checkpoint. Dst-side reads can't answer keys changed in the copy's own version (no seed).
  Commits on agents/g307-a626p3-final: 681800335 (fix + 2 replica tests + seeded oracle + seed file), then docs (bug
  record 2026-11-19-a-copy-from-before-a-checkpoint-hashed-otherwise-on-its-replica.md, design entry, GAPS fixed + new
  open "recount no oracle on a green folded past a copy") + copy_entry_room early return (uncommitted at 15:20).
  Red copy agents/g307-a626p3-red d2a2b0afc (7d08dd16a + tests). jupiter run r1512 (red copy_tests, green merge lib +
  anchor + fmt) started ~15:15 UTC. Worktrees: ~/slates-work/g307-a626p3-final, ~/slates-work/g307-a626p3-red.

- ~15:40 UTC after the Claude Code + kiln restart (r1512 lost): worktrees intact; docs + copy_entry_room early return
  committed as 617ad96e0 on agents/g307-a626p3-final (pushed; evidence still "pending" in the docs). Rerun = kiln r3
  (red d2a2b0afc copy_tests; green 617ad96e0 merge lib + anchor lib + fmt per crate). Main still 4644dcec3 = part 3's
  base, so the green run is the merged-tree run. NEXT: fill evidence (design entry, bug record, GAPS), one landing
  commit (cherry-pick -n 883f4468d acd417576 + f87d52b0f's resolution... simpler: squash agents/g307-a626p3-final's
  4 commits over main via a fresh worktree from origin/main and `git cherry-pick -n 7d08dd16a 681800335 617ad96e0
  <evidence>`), read the diff stat, push to main, clean jupiter + worktrees, tell main.

## Branches (all pushed to GitHub)
| piece | worktree | branch | head | state |
|---|---|---|---|---|
| A-663 | ~/slates-work/g307-a663 | agents/g307-a663 | 25da01c4c | code+tests+docs drafted; needs Studio red+green, evidence, land |
| A-663 red | ~/slates-work/g307-a663-red | agents/g307-a663-red | 91d14b44d | same test file as be9199db1 over main's engine |
| A-626 p3 | ~/slates-work/g307-a626p3 | agents/g307-a626p3 | 883f4468d | code+tests; needs Studio run, docs (§4.16 status, entry, GAPS), land |
| A-626 p4 | ~/slates-work/g307-a626p4 | agents/g307-a626p4 | 1413c2975 | code+server+tests pushed (over p3 + a663 merged in); needs docs (§4.16/§4.17.4 status, entry, GAPS: work model resolving copies owed + work shard leaves copy journals to owner), Studio run (slates-merge, slates-server, slates-anchor --lib), land after p3 |

## Studio runs to make (each in a fresh GitHub clone, own CARGO_TARGET_DIR under ~/.cache, deleted after)
1. A-663 red: `git clone -q --depth 1 -b agents/g307-a663-red https://github.com/hyper-light/slates.git ~/.cache/g307-red/src && cd ~/.cache/g307-red/src && CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR=~/.cache/g307-red/target cargo test --locked -p slates-merge --lib restate_tests` -- expect the 5 cases + oracle to fail (replay: "f names nothing"/"h is held"; case 1 wrong node).
2. A-663 green: same with -b agents/g307-a663, dir g307-green, then `cargo test --locked -p slates-merge --lib` whole + `cargo fmt -p slates-merge -- --check`; record census (run oracle with -- --nocapture? census is asserted, not printed).
3. A-626 p3 (after A-663 lands and origin/main merged in): `cargo test --locked -p slates-merge --lib` + fmt check (copy oracle prints its census with -- --nocapture: `cargo test -p slates-merge --lib every_copy_reads -- --nocapture`).

## A-663 design (as built)
- rebased_from restates effects in replay order (`in_replay_order` + `link_places`): each Hardlink moved to just before
  the first effect that unhomes its home (Effect::unhomes: SetContent Copied(from) -> from and dst; RemoveContent;
  RemoveSpecial), never before the last effect freeing the link's own name (Effect::frees); all else keeps order.
- Retarget (found writing the oracle, by reading compose_hardlinks): a link made where the head holds a link gets an
  Unlink first, unless the effects remove that link themselves (`removed_links`). Before: LinkOverExisting on resubmit.
- Oracle: crates/merge/src/engine/restate_tests.rs: real-inode model; four readings (own order over head; green
  accepting the work; restated journal replayed; green accepting restated journal composed again); census of 6.
- Docs drafted: bug record docs/bugs/2026-11-19-a-rebased-work-restated-its-links-after-moving-their-nodes.md,
  §4.16 rebase status, A-663 entry (end of SLATES_DESIGN.md), GAPS entry closed + 2 sibling entries; bug-claims
  marked [CLAIMED A-663 g307]. Fill "Evidence" after the runs; set [LANDED A-663 g307 <commit>] after landing;
  amendments.tsv row A-663 -> landed.

## A-626 part 3 design (as built, branch agents/g307-a626p3)
- checkpoint.rs: VERSION 4; `CopyRecord`, `Tables`, `Sum` (2,048 bytes = LtHash16 lanes LE), fields `copies`,
  `copied`, `sum`; decode refuses a repeated key/record (DuplicatePath); v3 BadVersion. Lib tests in
  src/checkpoint/tests.rs: golden (2,497 bytes, identity 13b881c8102bee2341e78a0d61ba6ffc1b5e90b31a082162f724c50c29251df3,
  derived by ~/.local/state/slates-fanout/g307/golden_checkpoint_v4.py with blake3_spec.py, a pure-Python BLAKE3 from
  the paper that reproduces the empty input, official vectors 1..3072 and A-646's 553a422f...), determinism, hostile.
- engine/copy_checkpoint.rs (new): copied_at (keys of copied trees at or before F with no own change in (copy, F],
  read at F; absent ones listed only if recorded), leave_out_copied, copy_records_at, sum_at (identity brought to head
  on a clone + branch's identity_moved_to), restore_copied (place_* without history; records + changes_bytes +
  last_changed), take_sum (identity := carried; copies_before := carried - restored state's sum).
- engine.rs: copied_by removed (checkpoint no longer None for copies); restore_* refactored onto place_*;
  head_identity_from_scratch skips records <= deltas_from and adds copies_before. branch.rs: pub(super)
  identity_moved_to. copy.rs: every_key pub(super). identity.rs: SUM_BYTES, add_sum, less, is_empty, to/from_bytes.
- Tests: copy_tests: a_green_holding_a_copy_is_checkpointed_and_restored (replaces the refusal test); the copy oracle
  restores every history's green at every version from its encoded checkpoint and replays (census checkpointed_copies,
  copied_keys). tests/checkpoint.rs (main's): every_table + presence_at arithmetic updated for v4.
- For main: a v3 checkpoint in an older segment now fences its green CheckpointCorrupt (verbs.rs) -- as A-321's v3 did;
  no anchor LAYOUT_VERSION bump made (main's call).
- Owed (GAPS, to write): a restore that grafts a tree whose source is unchanged since its version (checkpoint costs the
  copied trees key by key today).

## Main's rulings (2026-11-19)
- Part 3: bump crates/anchor LAYOUT_VERSION 11 -> 12 (done, acd417576), in its applied-to list; file the sibling
  (A-621/A-646 changed persisted formats with no bump; covered by 12) and the owed structural guard (done, GAPS +
  bug-claims). Touches to branch.rs/copy.rs fine.
- Part 4, option (a), the whole loop: LoggedOp::Copy in crates/server/src/work_log.rs's codec with LAYOUT_VERSION 13 in
  that landing (part 3 lands 12 first; one bump per landing, each in its applied-to list); every server arm real
  (work_home.rs:471, listing.rs:160, verbs.rs at both sites ~8353 and ~8452, work_charge.rs), no placeholder arm; the
  work shard's A-247 per-path base (Base::for_names / path_base) refuses a journal holding a Copy, typed and counted, the
  refusal named in GAPS beside the owed item (the work model resolving copies, so Rebased::changed sees a work's
  untouched copied files); rebased_from restates a copied-tree edit (SetContent(Copied(src)) under a CopyTree dst) as
  edits after its Copy (and link_places must not treat it as unhoming src); the deriver takes the copied tree's
  entries from Green::base_for, re-rooted, at the copy's version; oracle covers a copy, edits inside the copied tree, a
  rename out of it and a link into it (both refused TypeChanged, A-626 part 1), census of each. Build only after parts
  1 and 3 land and the Studio is back; writing it before is fine.
- A background watch (Bash bl46sz89j) prints STUDIO ONLINE when d1323898 returns.

## A-626 part 4 design (to build; branch from agents/g307-a626p3 with agents/g307-a663 merged in)
- VolumeOp::Copy { from, to, version, replace } in increment.rs; compose to OpKind::Copy (path to, src from, at version,
  REPLACE flag); ops beneath `to` in the same journal need the copied tree's files: Base must carry the source tree at
  `version` re-rooted (Green::base_for to add them per Copy op).
- rebased_from: CopyTree -> VolumeOp::Copy; a SetContent(path, Copied(src, edits)) with `path` inside a CopyTree's dst
  is a copied-tree edit (restate as content ops on path after the Copy), NOT a Rename; Effect::unhomes must not treat
  it as a move. Remove Green::rebase's TypeChanged refusal of copies.
- Oracle over generated histories with copies (rebase vs submit; restated journal composed again and submitted).

## Siblings found by reading (filed in GAPS on the A-663 branch, not fixed)
1. Deriver: `rm x; touch x` / made file renamed over x compose to an in-place edit of x's node (a link reads new bytes).
2. Deriver: `ln g l; mv f l` composes to Link l + Rename l<-f; the green holds a file and a link at l.
3. rebased_from: retargeted symlink restated without its removal (SymlinkOverExisting on resubmit).
4. rebased_from: directories restated after the files made in them.

## A-626 part 4 as built (agents/g307-a626p4)
- merge: increment.rs VolumeOp::Copy {from,to,version,replace} (footprint/content/names/single_path arms), check_copies
  (an op in a copy's tree before it -> Unsupported), emit_copies (by destination, REPLACE flag), `within`;
  engine/copy_restate.rs (new): Green::copied_base (drop_tree + push_copied: source tree at the copy's version
  re-rooted), restate_sourced (a SetContent(Copied) under a restated copy -> edits over the copied bytes, else the
  rename), copy_roots/copied_edit (link_places skips copied edits); engine.rs: base_for adds copied trees per Copy op,
  rebase's copy refusal removed, rebased_from's CopyTree arm -> VolumeOp::Copy (copies vec), SetContent(Copied) arm ->
  restate_sourced; copy_tests' old refusal test removed.
- server: work_log.rs LoggedOp::Copy appended last (+ of/op arms + round-trip list); work_home.rs holds_copy +
  COPY_COMPOSED_BY_OWNER ("merge.work.copy_composed_by_owner"): base_missing/composed leave a copy-holding journal
  to the owner; verbs.rs judge_submission/map_rebase count it; listing.rs Copy arm (replace drops declared dirs and
  symlinks in the destination's tree). verbs.rs:8353 matches WorkOp (no copy verb) and :8452 is a SetXattr find_map:
  no arm needed (tell main). anchor LAYOUT_VERSION 13.
- tests: increment/tests.rs (copy composes; refused before; by destination); engine/copy_restate_tests.rs (unit + oracle,
  census copies/edits/creates restated, renames out / links in refused).

## Main's rulings, part 4 (2026-11-19, later)
- Both departures approved: no verbs.rs arms; the owner composes a copy-holding journal (no refusal). Keep the counter
  merge.work.copy_composed_by_owner AND write a slates-server --lib test asserting it moved (TODO on resume: a work
  whose journal holds a copy, submitted or rebased, counts it); the owner's extra cost is in the A-626 part 4 entry.
- Background watch stopped on main's order (each wake burns allowance). Main messages when the Studio answers: that is
  the signal to resume (A-663 red/green -> land; p3 -> land; p4 + counter test -> land).

## Relaunch 2026-11-19 ~16:20 UTC (fresh g307; main e3234e618 = 4644dcec3 + 71f05f93d + e3234e618)
- Worktrees g307-a626p3-final (617ad96e0) and g307-a626p3-red (d2a2b0afc) intact, clean, pushed.
- jupiter run r84 (one script, target ~/.cache/g307-target, logs ~/.cache/g307-logs/*.log on jupiter): red copy_tests
  at d2a2b0afc; green slates-merge lib + copy census + slates-anchor lib + fmt (merge, anchor) at 617ad96e0; then
  main's `cargo test -p slates-merge --test engine` (piece 2, hard_link_merges at engine.rs:661).
- Meanwhile: squash part 3 over e3234e618 in a fresh worktree (main's 71f05f93d touches engine.rs and copy.rs).
- Piece 2 next: hard_link_merges (crates/merge/tests/engine.rs) -- link l -> other at base 2, nothing at other.
- r84 RED done 17:39:53Z (d2a2b0afc, Windows x86_64 jupiter, 1440 s incl. build): copy_tests 22 passed, 3 failed =
  a_replica_replaying_a_copy_edited_in_its_own_version_hashes_as_its_owner (copy_tests.rs:1301 identity),
  a_replica_restored_after_a_copys_source_version_hashes_as_its_owner (:1270 identity at 5), the copy oracle (:2079 via
  :2176; minimal input Put, Mkdir, Put, Copy{back 215, replace: true}). Green clone 617ad96e0 at 17:45:34Z.
- Squash staged (not committed) in ~/slates-work/g307-a626p3-squash (agents/g307-a626p3-squash, over e3234e618; text
  auto-merge clean; main's 71f05f93d Option bytes / commit(post_state) / mapped_files_made touch nothing part 3 adds).
  Main since moved to 9a7339e9e (svn + docs only; slates-merge depends only on blake3).
- Piece 2 written: agents/g307-hardlink-test 8d7209388 (over 9a7339e9e, pushed): test gives `other` a node (made at v1)
  so the differing link stays CreateCreate; adds link to `missing` at base 2 -> DeleteModify; merge_hardlink/node_home
  doc comments corrected. Engine right (A-646 eadd8e96a resolves the node before the name; name check compares homes).
  Sibling (report, not fixed): node_home Ok for a file made at the target AFTER the base where none stood at the base
  (file_removed_since false) -> a forged Base link names a node its work never saw.
- r84 GREEN (617ad96e0): slates-merge lib 182/183 -- the one failure the copy oracle census (nested_replaced 0; census
  rerun alone passed with 3); slates-anchor lib 31/31 (1283 s); fmt clean merge+anchor. main-engine-it at e417bfc72:
  48 passed, hard_link_merges FAILED engine.rs:661 left Some(DeleteModify) right Some(CreateCreate) (confirms reading).
- MAIN ~18:50Z (CLAUDE.md §8 one piece per commit): (1) A-672 census fix alone, test-only, green slates-merge lib + fmt,
  land FIRST; (2) part 3 lands on a green run after it; (3) hard_link_merges its own commit, NO suite run by me (main
  runs --test engine after landing); no reruns/seed replays/mutation red-checks. g306 dropped A-672 (amendments.tsv).
- Branches now: agents/g307-a672 0d624fe70 (over main bf9d39139: fixed_history 9 steps, Census::met 11 cases, oracle
  runs it first; A-672 design entry (evidence to add at landing); GAPS line in A-668's paragraph);
  agents/g307-a626p3-over672 5d7b2dda2 (part 3 over A-672: met() +2 checkpoint cases; part 3 docs re-attribute census
  to A-672); agents/g307-p3o672-hardlink 672f2fabf (+ piece 2). Superseded scratch: agents/g307-a626p3-squash 808aa087f,
  agents/g307-p3-hardlink b254bcfc7 (never landed).
- jupiter r188 (started ~18:58Z): A-672 lib (--nocapture for the census) + fmt. NEXT: land A-672 (fresh worktree from
  origin/main, cherry-pick -n 0d624fe70, add evidence bullet, one commit, push); then run part 3 stacked tree
  (agents/g307-p3o672-hardlink: slates-merge lib + fmt merge + fmt anchor), land part 3, then piece 2.

## Relaunch 2026-11-19 ~19:15Z (fresh g307; brief: A-672 alone, Studio, then END)
- Main's correction (Ada, ~19:20Z): run FOCUSED unit AND integration tests (filtered), land on green of both; only whole
  suites are out. A-672: no integration target compiles copy_tests (a #[cfg(test)] module of the lib; none of the 15
  targets in crates/merge/tests names a copy test), so its focused run is the filtered lib + fmt. hard_link_merges
  (later): `cargo test --locked -p slates-merge --test engine hard_link_merges`.
- Census on origin/main counts 11 cases (A-672 meets all 11); A-626 part 3 adds 2 (checkpointed_copies, copied_keys) = 13.
- Run branch agents/g307-a672-run c2e1cc8d2 (pushed) = origin/main cd84e3811 + 0d624fe70 cherry-picked; the design-doc
  conflict (main appended A-661 part 1 and A-616 (6)) resolved: main's entries, then A-672's; copy_tests.rs identical
  to 0d624fe70's. Worktree ~/slates-work/g307-a672-run.
- Studio kiln r4 (d1323898, macOS arm64) started ~19:25Z: clone ~/.cache/g307-a672-src, target ~/.cache/g307-target,
  logs ~/.cache/g307-logs/a672-{lib,fmt}.log; `cargo test --locked -p slates-merge --lib copy -- --nocapture` + fmt.
- Landing worktree ~/slates-work/g307-a672-main (agents/g307-a672-main, over cd84e3811): cherry-pick -n c2e1cc8d2
  staged, uncommitted; NEXT: evidence bullet in the A-672 entry (+ GAPS), one commit -F, fetch+merge, diff stat,
  markers, push HEAD:main; then bug-claims line 86 -> [LANDED A-672 g307 <id>], amendments.tsv A-672 row -> landed,
  run-requests (Windows x86_64 + Linux owed), delete Studio clone + target, remove a672 worktrees, report to main, END.
- A-672 LANDED on main 5c0a522f1 (58bd11087 = the one commit: copy_tests.rs fixed_history + Census::met, the A-672
  design entry with Evidence, GAPS A-668 line; merge 5c0a522f1 of origin/main 0c7a77bd3, CLAUDE.md-only). Evidence:
  macOS arm64, Mac Studio, kiln r4 (2026-11-19T19:17:45Z, load ~3.5/32 cores), over cd84e3811 at c2e1cc8d2 (same code):
  `cargo test --locked -p slates-merge --lib copy -- --nocapture` 34 passed, 0 failed, 146 filtered out (24 s incl.
  build); fixed history census accepted 3, refused/from_root/from_before/into_themselves/from_changed/changed_beneath/
  replaced/nested/nested_replaced/moved 1 each; whole run accepted 89, refused 438, from_root 16, from_before 53,
  into_themselves 27, from_changed 25, changed_beneath 18, replaced 37, nested 35, nested_replaced 4, moved 21;
  `cargo fmt -p slates-merge -- --check` clean. No integration target compiles copy_tests (none ran). Windows x86_64 +
  Linux owed (run-requests.md 19:25Z). bug-claims line 86 -> [LANDED A-672 g307 58bd11087]; amendments.tsv A-672 row
  -> landed (backup amendments.tsv.bak-g307-a672b). Studio clone + ~/.cache/g307-target deleted (logs kept in
  ~/.cache/g307-logs on the Studio); worktrees g307-a672, g307-a672-run, g307-a672-main removed (branches pushed or on
  main: agents/g307-a672 0d624fe70, agents/g307-a672-run c2e1cc8d2).
- NEXT (main starts each): (1) A-626 part 3 from agents/g307-a626p3-over672 5d7b2dda2 (worktree
  ~/slates-work/g307-a626p3-over672): it carries 0d624fe70, so `git merge origin/main` will likely conflict in the
  A-672 design entry + GAPS A-668 line: take main's (with evidence); its met() adds 2 checkpoint cases (13 in all);
  focused runs: slates-merge --lib <filters> (copy, checkpoint) + `--test checkpoint` filtered + slates-anchor --lib
  filtered + fmt. (2) hard_link_merges: agents/g307-hardlink-test 8d7209388 (or p3o672-hardlink 672f2fabf stacked);
  run `cargo test --locked -p slates-merge --test engine hard_link_merges` (+ lib filter if engine.rs doc comments
  change) + fmt. Superseded worktrees still on disk: g307-a626p3-squash, g307-p3-hardlink (pushed; remove when done).

## A-626 part 3 (main ~19:35Z: next piece, alone; land on green as one commit + the merge; then END)
- Run branch agents/g307-a626p3-run (worktree ~/slates-work/g307-a626p3-run), pushed: 37f1e87ae = cherry-pick of
  over672's 5d7b2dda2 onto main 5c0a522f1 (merge+anchor code identical to 5d7b2dda2; design conflict resolved: main's
  A-672 Evidence bullet, then part 3's entry; GAPS auto-merged), be0348459 = new anchor integration test
  `a_segment_from_before_version_4_checkpoints_is_refused_at_start` (layout 11 refused at attach; A-539's pattern; no
  test held part 3's bump before). Chose cherry-pick over merging main into over672: that merge would land the
  pre-landing A-672 (0d624fe70) on main as a duplicate commit.
- Focused tests (r40, Studio): merge lib `-- copy checkpoint engine::branch engine::node_tests engine::reanchor
  engine::stack::tests --nocapture` (every lib caller of checkpoint/restore/recount/fold/identity_moved_to); merge
  `--test checkpoint checkpoint` (every_table holds a CopyRecord), `--test chain_replay checkpoint` (restore, no copies);
  anchor `--lib layout`, `--test anchor a_segment_`; fmt merge + anchor. Logs ~/.cache/g307-logs/p3-*.log on the Studio.
- NEXT: landing worktree from origin/main, cherry-pick -n 37f1e87ae be0348459, docs (applied-to += anchor.rs test;
  Tests bullet; Evidence: Studio r40; GAPS "macOS owed" -> r40), one commit -F, merge, diff stat, markers, push.
- Main's "checkpoint format unchanged" = the fix's (no recorded entries); part 3 itself is checkpoint v4 + layout 12.
- A-626 part 3 LANDED on main 2e4433040 (637a65d69 = the one commit: part 3's 14 files from over672's 5d7b2dda2 +
  crates/anchor/tests/anchor.rs `a_segment_from_before_version_4_checkpoints_is_refused_at_start` (new; layout 11
  refused at attach, A-539's pattern) + docs evidence; merge 2e4433040 of a6306963e). Evidence: macOS arm64, Mac Studio,
  kiln r40 (2026-11-19T19:28:44Z, load ~15/32), over 5c0a522f1 at be0348459 (= landed code): merge lib `-- copy
  checkpoint engine::branch engine::node_tests engine::reanchor engine::stack::tests` 73/73 (112 filtered out); census
  fixed history all 13 met (checkpointed_copies 6, copied_keys 5), whole run 97/395/23/55/34/19/45/36/40/7/28/337/335;
  `--test checkpoint checkpoint` 4/4; `--test chain_replay checkpoint` 3/3; anchor `--lib layout` 2/2; `--test anchor
  a_segment_` 5/5; fmt merge + anchor clean. Main's commits 5c0a522f1..a6306963e touch none of the 12 crates in the
  merge+anchor closure (Cargo.toml/lock: slates-hg for slates-server only), so r40 stands for the merged tree.
  Design-doc conflicts resolved twice (main's A-653 then A-657 part 3 entries first, part 3's last).
  Bookkeeping: amendments.tsv A-626 row (backup .bak-g307-a626p3), bug-claims line 75 [COVERED ...], run-requests
  19:40Z (Linux + Windows merged tree owed; mutation red of the anchor test owed). Studio clone + target deleted; all
  part 3 worktrees removed (branches on GitHub: agents/g307-a626p3-run be0348459, -final 617ad96e0, -red d2a2b0afc,
  -squash 808aa087f, -over672 5d7b2dda2, agents/g307-p3-hardlink b254bcfc7, agents/g307-p3o672-hardlink 672f2fabf).
- NEXT (main starts it): hard_link_merges from agents/g307-hardlink-test 8d7209388 (worktree
  ~/slates-work/g307-hardlink-test, over 9a7339e9e): fresh worktree from origin/main, cherry-pick -n 8d7209388 (test in
  crates/merge/tests/engine.rs + merge_hardlink/node_home doc comments in src/engine.rs), run
  `cargo test --locked -p slates-merge --test engine hard_link_merges` + fmt (+ a lib filter only if code changes),
  one commit + merge. Sibling to report (not fix): node_home Ok for a file made at the target after the base.

## hard_link_merges (main ~19:45Z: next piece, alone; red on main's test, green on mine; one commit + the merge; END)
- 8d7209388 (agents/g307-hardlink-test, over 9a7339e9e): test fix in crates/merge/tests/engine.rs + doc comments only
  in src/engine.rs (merge_hardlink, node_home); no lib test touched; no docs/GAPS/bug-claims entry names the test.
- Run branch agents/g307-hardlink-run 472282853 (pushed) = 2e4433040 + cherry-pick 8d7209388 (clean auto-merge).
  Studio kiln r74: red = clone of main (newest at clone time), green = run branch, separate targets
  (~/.cache/g307-hl-red-target, ~/.cache/g307-target), fmt; logs ~/.cache/g307-logs/hl-*.log.
- NEXT: in ~/slates-work/g307-hardlink-test merge origin/main with an evidence-carrying message, check its tree
  equals 472282853's for crates/merge (main's newer commits must not touch the merge closure), diff stat (2 files),
  markers, push HEAD:main; remove worktrees g307-hardlink-run, g307-hardlink-test; delete Studio clones + targets.
- Sibling to report to main (not fixed, by reading): node_home answers Ok for a file made at the target after the
  base where none stood at the base (file_removed_since false), so a forged Base link would name a node its work
  never saw.
- hard_link_merges LANDED on main 1b480bd70 (8d7209388 = the fix, its message saying why the expectation changed;
  merge 29d41e84d of 3b357a256 carrying r74's red/green; merge 1b480bd70 of 71ebf2a74 after the first push was refused,
  main's new commits touching nothing of crates/merge or test-seeds). Evidence: macOS arm64, Mac Studio, kiln r74
  (2026-11-19T19:42Z): red on main's test at 3b357a256, `cargo test --locked -p slates-merge --test engine
  hard_link_merges` FAILED engine.rs:661 left Some(DeleteModify) right Some(CreateCreate); green at 472282853
  (2e4433040 + fix, crates/merge identical to the landing) 1 passed; fmt clean; no lib test touched (doc comments only).
  Filed: bug-claims 19:55Z (node_home Ok for a file made at the target after the base, unclaimed); run-requests 19:55Z.
  Studio clones + targets deleted; worktrees g307-hardlink-run, g307-hardlink-test removed (branches on GitHub:
  agents/g307-hardlink-run 472282853, agents/g307-hardlink-test 8d7209388 + merges = main). No g307 worktrees remain.
- NEXT (main starts it): A-626 part 4 from agents/g307-a626p4 1413c2975 (pushed; built over an older part 3 acd417576
  with A-663 merged in): make a fresh worktree from origin/main and cherry-pick part 4's own commits only (part 3 landed
  as 637a65d69 with different history, so merging that branch would bring old part 3 commits); main's part 4 rulings
  above (LoggedOp::Copy appended last, anchor LAYOUT_VERSION 13 now that part 3 made it 12, the
  merge.work.copy_composed_by_owner counter + a slates-server --lib test that it moves, docs owed); focused runs:
  slates-merge lib filters (increment, copy_restate, copy), slates-server --lib work_log/work_home filters, anchor layout
  + a layout-12 refused-at-start test like part 3's, fmt.

## A-626 part 4 started, then PARKED for main's regression piece (2026-11-19 ~19:50Z)
- Part 4 carried onto main acc2ac16f: agents/g307-a626p4-run eb304049c (pushed; worktree ~/slates-work/g307-a626p4-run):
  part 4's own commits 76def2659 efbe731ca 1413c2975 e514f7063 ec96ec105 cherry-picked; engine.rs conflicts resolved
  (main's handed_in kept beside part 4's copies; restate_sourced passed `bytes.unwrap_or_default()`, main's Option
  bytes); GAPS joined; design doc dropped the old A-663 draft. NOT RUN yet. Owed before landing: the server lib test that
  merge.work.copy_composed_by_owner moves (a SealedWork with a Copy in its journal via crate::verbs::owner_rebase inside
  crate::daemon::audit_on_shard, green via dispatch(CreateGreen); count read from state.refusals), a layout refused-at-start
  test, and its layout now 14 (the regression fix takes 13), docs evidence.

## Regression from A-626 part 3 (main ~19:55Z: before part 4)
- slates-server repository::tests::each_verb_moves_the_names_log_and_each_members_version_its_own_log fails on main since
  637a65d69. ROOT CAUSE (measured, kiln r122, diag prints in a clone): checkpoint v4 wrote the 2,048-byte identity sum
  into EVERY checkpoint; merge_service::checkpoint_green_when's doubling rule (take a checkpoint when folded bytes >= held
  checkpoint bytes) then waited: checkpoint at 1 = 2,233 bytes, second write folds 194 < 2,233, so durable floor stayed 1
  and the branch at 1 was made (GreenCreated, test wants UnknownBase).
- FIX (A-626 part 3, continued (1)): checkpoint version 5: copies' section and sum each behind a presence byte, written
  beside a copy's record alone (Checkpoint::sum: Option<Sum>); Green::checkpoint computes sum only with copies; restore
  takes it only when present; anchor LAYOUT_VERSION 13 + a_segment_from_before_version_5_checkpoints_is_refused_at_start;
  goldens re-derived (2,499 bytes 05c1e7f6..., never-copied 115 bytes dfa590d1..., script
  ~/.local/state/slates-fanout/g307/golden_checkpoint_v5.py); part 3's replica test compares sum_at(3) + whole checkpoints.
- Branches: agents/g307-p3-sum-fix 189ab8dc3 (code) + e52b852ba (docs) over 545181b30; r146 there all green
  (server test 1/1, checkpoint at 1 now 155 bytes, at 2 taken 195; merge lib 76/76; --test checkpoint 4/4; chain_replay
  3/3; anchor layout 2/2, a_segment_ 6/6; fmt clean). Main moved (A-676 engine.rs node_home, ipc protocol, server
  submit message) -> landing tree agents/g307-p3-sum-land 8919290ed over b13a6e05f (worktree ~/slates-work/g307-p3-sum-land),
  rerun kiln r164 in flight. NEXT: if green, fresh worktree from origin/main, cherry-pick -n 8919290ed, commit -F
  ~/.local/state/slates-fanout/g307/commit-msg-sumfix.txt (add r164), merge if needed, diff stat, markers, push; then
  part 4 (layout 14).
- REGRESSION FIX LANDED: main 7d020df47 (5f27f8cf9 + merge of 2dc39bd60). r146 + r164 green (see run-requests 20:35Z).
  Worktrees g307-p3-sum-{fix,land,final} removed; Studio g307-fold-* deleted. Reported to main ~20:38Z.

## A-626 part 4 resumed (~20:40Z) over main 7d020df47
- Worktree ~/slates-work/g307-a626p4-v2, branch agents/g307-a626p4-v2, scratch ee6b17841 (pushed) = eb304049c
  cherry-picked: conflicts resolved (layout.rs -> 14 keeping 13's line; engine.rs rebase: A-658's immutable rebase kept,
  copy refusal dropped; GAPS + §4.16 status + end entries joined); new tests: work_home.rs
  a_journal_holding_a_copy_is_composed_by_the_owner_and_counted (audit_on_shard, CreateGreen, owner_rebase of a
  SealedWork whose journal copies the root at 0 to b; counts [0,1,1]); anchor
  a_segment_from_before_logged_copies_is_refused_at_start (layout 13). Part 4 entry: layout 14; evidence pending.
- Studio kiln r193: merge lib `-- increment copy restate rebase base_for`; --test increment/rebase/base_for whole;
  server lib `-- work_log work_home listing --test-threads=4`; anchor layout + a_segment_; fmt merge/server/anchor.
- Old part 4 worktree ~/slates-work/g307-a626p4-run (eb304049c, pushed) superseded: remove after landing.
- r193 (ee6b17841): merge lib 61/62 (part 4 oracle failed: rebase+submit refused ALIKE [TypeChanged, TypeChanged,
  DeleteModify] on "out" for copy trunk->b + edit + rename out; oracle demanded TypeChanged only); --test increment
  42/42, rebase 2/2, base_for 3/3; server lib 6/7 (counter test asserted a clean rebase; the root copy at v0 is
  answered a conflict on "b", class 7); anchor layout 2/2, a_segment_ 7/7; fmt server needed a split assert.
- Fix 1541a1078 (pushed): refused_typed = any TypeChanged (windows equal); fixed test
  an_edit_then_a_rename_out_of_a_copy_is_refused_alike (raw ((0,0,false),[(188,150),(130,81)])); counter test asserts
  counts [0,1,1] whatever the owner answers the copy (work model resolving copies owed) + directory rebases clean;
  design entry updated. r220 (finished before the 20:4xZ restart): merge lib 63/63, server lib 7/7, fmt clean.
- Main moved 7d020df47 -> bb535bd4d with A-677 (crates/merge engine.rs +30, node_tests, tests/rebase.rs) and server
  changes: NEXT = fresh worktree from origin/main, cherry-pick -n ee6b17841 then 1541a1078, resolve, scratch commit,
  push, Studio rerun (merge lib -- increment copy restate rebase base_for; --test increment/rebase/base_for; server lib
  -- work_log work_home listing --test-threads=4; anchor layout + a_segment_; fmt x3), then the final commit (fresh
  worktree, cherry-pick -n, message with evidence, merge, diff stat, markers, push), bookkeeping, END.

## Relaunch 2026-11-19 ~21:25Z (fresh g307; brief: confirm A-626 part 4, then the repository test's margin)
- A-626 part 4 LANDED on main 3612d1ef9 (one commit over d015143ac, by the predecessor at ~21:07Z): crates/ patch-id
  equal to the tested scratch 15ba6b078 (r40 over ee5cbed4c; main's ee5cbed4c..d015143ac touch crates/git and crates/svn
  alone); docs carry r15 + r40 evidence (§4.16, §4.17.4 status, GAPS, the part 4 entry). r40 logs on the Studio
  (~/.cache/g307-logs/p4d-*): merge lib 67/67, --test increment 42/42, rebase 3/3, base_for 3/3, server lib 7/7,
  anchor layout 2/2, a_segment_ 7/7, fmt clean.
- Bookkeeping done now: amendments.tsv A-626 row -> part 4 landed (backup .bak-g307-a626p4); run-requests 21:30Z
  (Windows x86_64 + Linux owed; backup .bak-g307-a626p4); worktrees g307-a626p4-{final,land,landing} removed (branches
  agents/g307-a626p4-final 15ba6b078, agents/g307-a626p4-land 3c5ad384f on GitHub; landing = main). Studio: no g307
  clone or target left (only ~/.cache/g307-logs).
- Stale by its date, not fixed (told main): SLATES_DESIGN.md's A-626 part 3 continued (1) entry ends "For A-626 part 4,
  not yet landed: its logged copy takes the anchor's layout 14."
- NEXT (this brief): slates-server repository::tests::each_verb_moves_the_names_log_and_each_members_version_its_own_log
  derives its expectation from merge_service's doubling rule's own inputs (held checkpoint bytes vs bytes to fold), so
  a checkpoint format change cannot flip it unseen (39 bytes of margin today: 194 to fold vs a 155-byte checkpoint).
- Fold-margin piece (no A-number; test robustness + a behaviour-preserving extraction): worktree
  ~/slates-work/g307-fold-margin, branch agents/g307-fold-margin, scratch ae9d418a7 (pushed) over main 3612d1ef9.
  merge_service.rs: FoldInputs {floor, to_fold, held_bytes} + waits() + fold_inputs(state, green), read by
  checkpoint_green_when (no behaviour change). repository.rs tests: write_until_folded writes the root until a
  checkpoint passes the one held, asserting while none is taken that the rule's inputs say wait and to_fold grew (ends
  within held_bytes writes); the test's root head, topic/lone serials follow it. Bug record's Siblings bullet updated.
  Sibling sweep (server unit + integration, client tests): none other leans on the margin.
- Studio kiln r105 (21:37:48Z, load 12.3): `cargo test --locked -p slates-server --lib -- repository::
  a_green_replays_from_its_checkpoint --test-threads=4`; `--test green_checkpoint -- --test-threads=4`; fmt server.
  Logs ~/.cache/g307-logs/fm-*.log; clone ~/.cache/g307-fold-src; target ~/.cache/g307-target (delete both at END).
- NEXT: on green, fresh worktree from origin/main (agents/g307-fold-margin-land), cherry-pick -n ae9d418a7, commit -F
  ~/.local/state/slates-fanout/g307/commit-msg-fold-margin.txt with r105's numbers, merge if main moved, diff stat (3
  files), markers, push HEAD:main; run-requests (mutation red: held_bytes + 64 in fold_inputs -> old test GreenCreated,
  new passes; Windows/Linux); delete Studio clone + target; remove worktrees; report; END.
- FOLD-MARGIN LANDED on main 3b0b95379 (1aa90e950 = the one commit: merge_service FoldInputs/fold_inputs/waits read by
  checkpoint_green_when; repository.rs write_until_folded; bug record Siblings bullet; merge 3b0b95379 of f05af5288).
  Studio r105 (over 3612d1ef9), r150 (34d10acb5), r170 (225d7eead = landing code): lib filtered 25/25, --test
  green_checkpoint 6/6, fmt clean. run-requests 22:10Z (mutation red + Windows/Linux owed). Worktrees removed
  (branches agents/g307-fold-margin 5f3f830a5 scratch, agents/g307-fold-margin-land = main). Studio target
  ~/.cache/g307-target KEPT for the svn piece; clone ~/.cache/g307-fold-src to delete.
## Subversion interop tests (main ~21:50Z redirect + ~22:05Z spellings): crates/cli/tests/svn_interop.rs
- Stock svn 1.15.0 (Studio) checks out, commits, updates, locks over svn:// (--vcs-svn, g234), http (--vcs-http, Basic
  + token on loopback with Basic's challenge first: A-686 g083) and svn+ssh (--vcs-ssh: A-684 g299; svnserve -t: g229);
  repository by `slates repository create <name> --tool svn` (A-685 g305); commit through the store g268; ra_svn lock
  verbs answered 210001 on main (serve.rs) -- tell main. Ignore only for pieces in flight, naming piece + agent.

## Relaunch 2026-11-19 ~22:40Z (fresh g307; the svn interop piece, the predecessor stopped ~22:35Z with nothing written)
- Predecessor left no svn branch, worktree, scratch file or Studio run (checked: ls ~/slates-work/*307*, ls-remote
  agents/g307*, Studio ps/~/.cache). Studio keeps ~/.cache/g307-target (reuse) and ~/.cache/g307-fold-src (delete).
- Measured (Studio, 22:24Z, stock svn 1.15.0 vs svnserve -t through `config:tunnels:control=sh -c 'exec svnserve -t
  -r ROOT --tunnel-user=U'`, URL svn+control://localhost/r): checkout "Checked out revision 0.", commits "Committed
  revision N.", update "Updated to revision 2.", log --xml -v entries, lock "'file' locked by user 'U'.", info "Lock
  Owner: U", a commit from another copy under the lock E160037 exit 1 head unchanged, unlock "'file' unlocked.".
- PLAN: crates/cli/tests/svn_interop.rs (#![cfg(unix)]): one round trip + one lock session; control test runs both
  against stock svnserve (not ignored; skips loudly without svn 1.15/svnadmin/svnserve); slates legs ignored naming
  pieces: svn:// (A-689 + lookup g234, A-685 g305, A-655 2b g268, CRAM-MD5 secrets unassigned -> tell main), http
  round trip (A-681 g297, A-686 g258, A-685, A-655 2b), http locks (A-681's LOCK g297 ...), svn+ssh (A-684 g299,
  A-688 g229, A-685, A-655 2b). ra_svn lock verbs unserved (210001), no piece -> tell main; no lock leg there.
  Docs: §4.17.4 item 14 status blockquote; GAPS paragraph at the end of the §4.17.4 block.
- Worktree ~/slates-work/g307-svn-interop, branch agents/g307-svn-interop (from origin/main).
- SVN INTEROP TESTS LANDED on main e8fbe9116 (646e9deab = the one commit: crates/cli/tests/svn_interop.rs new, §4.17.4
  item 14 status blockquote, GAPS paragraph at the end of the §4.17.4 block; merge e8fbe9116 of 550153532, message
  amended before the push). Evidence: macOS arm64, Mac Studio, kiln r293 over main e25799e8c (22:40-22:45Z):
  `cargo test --locked -p slates-cli --test svn_interop` 1 passed (the control vs stock svnserve -t, 11.2 s), 4
  ignored; `-- --ignored --test-threads=2` 4 failed at their first missing piece (--vcs-svn/--vcs-ssh unknown flags,
  `repository create` exit 2 "unknown flag `--tool`"); `cargo fmt -p slates-cli -- --check` clean. run-requests
  22:58Z (clippy/xtask; Linux with svn 1.15; un-ignore per leg as pieces land). Studio clone + g307-target deleted
  (logs kept in ~/.cache/g307-logs); worktrees g307-svn-interop, g307-svn-interop-land removed (scratch branch
  agents/g307-svn-interop ad7beb38b on GitHub; landing = main).
- Ignores: svn:// (A-689 + its lookup g234, A-685 g305, CRAM-MD5 secrets: no piece; test assumes user = token id,
  password = token); HTTP round trip (A-681 g297, A-686 g258, A-685); HTTP locks (A-681's LOCK/UNLOCK); svn+ssh
  (A-684 g299, A-688 g229, A-685; port line assumed as the hg tests assume it). Told main: ra_svn lock verbs unserved
  (210001) with no piece; CRAM-MD5 secrets with no piece.
- NEXT: none (main starts the next piece).

## Main ~23:00Z: two new pieces, each one commit + merge, failing test first; report after each; END after both
1. A-692: svn:// authentication against enrolled credentials: CRAM-MD5 keyed contexts (HMAC-MD5 inner/outer MD5 states
   after the key, Dovecot's scheme) derived at `slates credential token` enrollment, kept beside the token's hash in
   the credential record, never the secret; ra_svn's CRAM-MD5 finishes the HMAC from them, proving the token's
   principal; username = token id. Integration: the svn:// leg if A-689 landed, else the server's ra_svn session test
   with a real credential record.
2. A-693: ra_svn's lock verbs (lock, lock-many, unlock, unlock-many, get-lock, get-locks) from the one lock record
   (A-602, A-656 continued (3)) in svnserve 1.15's answers (210001 today); held to the stock-svnserve control's
   transcript.
- A-692 IN PROGRESS (~23:35Z): worktree ~/slates-work/g307-a692 (agents/g307-a692 58ec2b234 = a93b8a016 code+tests +
  lock fix/docs); red ~/slates-work/g307-a692-red (agents/g307-a692-red a7824dfa7: of_secret carries no key,
  cram_md5_key keys no user). Design: CramContexts (Dovecot layout, outer then inner), MD5 from RFC 1321 in
  crates/svn/src/handshake/rfc1321.rs (md-5 0.10 hides state; hmac dropped, md-5 a dev oracle); CredentialKind::
  HttpToken { cram_md5_contexts }, CredentialMaterial::HttpToken { id, cram_md5_contexts }; server credentials::
  cram_md5_contexts/cram_md5_key/Unkeyed/CramUser; issuer derives contexts; anchor LAYOUT_VERSION 15 + refused test;
  svn_interop waits updated (A-685 landed; CRAM served). Golden by ~/.local/state/slates-fanout/g307/
  cram_contexts_golden.py. First Studio run r401 failed --locked (digest kept subtle); fixed in 58ec2b234; rerun
  kiln r416 (red then green; logs ~/.cache/g307-logs/a692-*.log; targets g307-a692-red-target, g307-target).
  NEXT: fill @@A692RUN@@ (design entry) and @@A692GAPSRUN@@ (GAPS) with r416, landing worktree from origin/main,
  cherry-pick -n a93b8a016 58ec2b234, one commit -F, merge, diff stat, markers, push; then A-693.
- A-692 runs: RED (stubs, agents/g307-a692-red a7824dfa7/6a50415a0): r416 slates-svn --lib handshake:: 7 failed of 37
  (rfc_2195 example, kept_contexts golden, lock-many auth, rfc_2202, stock cram sessions, contexts proptest,
  cram_c proptest); r450 server an_ra_svn_session FAILED (tests.rs:533). GREEN over 0462bd922: r416 svn handshake::+
  serve:: 64/64, db credentials 5/5, db + ipc wire_order 2/2 each, anchor layout 2/2 + a_segment_ 8/8, cli svn_interop
  1 passed 4 ignored, fmt x7 clean; r450 server credentials:: daemon_edge 9/9 (vcs_ssh filter wrong: it is
  vcs_ssh::site::tests). Landing tree agents/g307-a692-land e8be27b7d (cherry-pick -n of a93b8a016 58ec2b234
  d214a6b91 over main 9ca6e3883; GAPS + design conflicts resolved: interop waits, main's A-689 paragraph, A-692's;
  design: main's entries then A-692's). Run r456 over it. A-689 landed (listener answers No repository found): the
  svn:// leg stays ignored; integration = the server session test. g309 owns svn_interop's issuer-exports fix.

## Relaunch 2026-11-20 ~02:00Z (fresh g307; brief: A-692 then A-693)
- Predecessor's last state: branch agents/g307-a692-one (worktree ~/slates-work/g307-a692-one): a99f78d6e = the A-692
  commit over 9ca6e3883, then merges da1e2b6cf, 6566e41d3, af74663f6, e328c7d0c (main chased four times). Studio run
  a692q over e328c7d0c (01:37-01:39Z) all green: svn 64/64, db 5/5, db+ipc wire_order 2/2 each, anchor layout 2/2 +
  a_segment_ 8/8, server `-- credentials:: daemon_edge vcs_ssh::site vcs_ssh::svn --test-threads=4` 38/38, cli
  svn_interop 1 passed 4 ignored, fmt clean (7 crates). Main then moved to 3b392dcb1 (A-681's lender: slates-svn
  index/migration, server vcs_store/svn_view + vcs_http svn).
- ae208177f (pushed agents/g307-a692-one) = merge of 3b392dcb1; design-log end conflict: main's lender entry first,
  A-692's last; over main the piece's 23 files alone. Studio kiln r592 (logs ~/.cache/g307-logs/a692r-*.log, clone
  ~/.cache/g307-a692-r-src, target ~/.cache/g307-target) running the same focused set. NEXT: on green, push HEAD:main
  (merge again if main moved; rerun only if main's new commits touch the closure), bookkeeping, report, then A-693.
