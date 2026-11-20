# g307 — A-663 (rebase restates links by node), then A-626 parts 3 and 4 (UI goal 93, kiln 51cef9f7)

Started 2026-11-19 ~09:00 UTC. Supervisor: main. Implementer (THE RULE, §8): unit tests + fmt only, on the Mac Studio
d1323898 (adas-mac-studio.local-2), CARGO_BUILD_JOBS=4, one cargo at a time, target dirs deleted after reading.
Repo for worktrees: ~/slates-work/g066-fs-server/.git. Scratch: ~/.local/state/slates-fanout/g307/.
Studio OFFLINE from ~09:20 UTC (main's notice); checked every ~5 min with `kiln run true` (on adas-mac-studio.local-2).

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
