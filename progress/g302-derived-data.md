# g302 — A-647: A-645's derived data as one mechanism (UI goal 93, kiln 51cef9f7) — BOTH PARTS LANDED

Supervisor: main. Own clone ~/slates-work/g302-base; worktree ~/slates-work/g302-a647 (branch
agents/g302-a647-derived-data, merged into main). Local scratch ~/slates-work/g302-scratch (commit messages, stub
copies). Studio scratch ~/.cache/g302-a647 DELETED (1.7 GB). No build was run on the MacBook (fmt only).

## Decisions
- Crate: slates-vcs, module `derived` (+ `derived/paths.rs`, `derived/tests.rs`): values are content in slates-vcs's
  store (window, budget); (type, version) -> value is a slates-vcs Index row (same again `Identical`, other `Diverged`);
  git and hg already depend on slates-vcs, so no new crate rows.
- `Form::Derived`: a node naming contents and freeing them with itself (`put_derived`, `DerivedNode`); never names a table.
- Fill: one unit a step (found / walked / derived), for the server's `image::sliced` loop; waiting versions charged.
- Path map: one 16-way hash trie per directory (BLAKE3 place), a leaf while it fits the window, collapse when it fits
  again; canonical; the window in the type's name; derived by path copying through the change's directories only.
- Part 2: A-614's D(k) + reached set = `store::ReachSets` over `Graph for Repository`; each version records `Followed`
  (grown / remade from (green, version) / emptied) so derivation reads only recorded history; the store's chains and
  outputs unchanged; `Repository::reach_value` = the admission's sets as the value, kept with no second walk.

## Part 1 — LANDED df50dc58c (main ae8a9e38a..df50dc58c)
- d6a600d69 red (11 of 12 new tests failing vs stubs), e09601b71 code, ec31c6408/f43c6e71d/2e3bf9fb1/28517b1ac tests
  and refinements, c02cd4e88 docs. slates-vcs --lib 45/45, 46/46 merged; census ~1,300-1,450 versions, 130-160 merges,
  55-60 multi fills, ~133 splits, 24-31 collapses, 4 refusal kinds. Red six ways (no-collapse first missed, then caught).

## Part 2 — LANDED b982c336a (main 7d3e1db3b..b982c336a)
- 9b6c49357 red (3 of 4 new tests failing vs stubs; 442/442 existing slates-git tests pass with Followed recorded),
  67b1da27a code, 1cafc67a9 walk-cost check, e6d80fd8c docs. slates-git --lib 446/446 (447 merged), 3 ignored (main's).
  Red five ways (stop set, base dropped [caught only by the walk check], leftover parents, touching runs, empty D(k)).

## Run requests filed
- run-requests.md 2026-11-19T07:20Z g302: clippy (slates-vcs, slates-git), xtask check, doc gate, lint matrix.

## Left / owed (in A-647's entries and GAPS)
- The git store serving `brought`/`reached` from its values and dropping the chains' sets (signature change; wait for
  A-614's other parts to settle). A-553's node revisions and A-588's manifests as types. The server's Derivations per
  repository on its owner shard. Dropping values no version needs; sliced release and large derivations; a green's
  maps (A-534) as a path map; values offered between tiers by identity.

## Main's routing (2026-11-19, after A-647 landed)
- The server's per-repository Derivations: NOT mine; g258's A-662 (the ShardState map holding each repository's tool
  store and its derived index on the owner shard). Do not build it.
- The git store serving brought/reached from its values: land only after g230's A-644 is on main (main says when).
- A-553's and A-588's stores as types: only after g300's A-649 is on main.

## A-666 part 1 — LANDED 0e755c9b1 (main 4b12a582e..0e755c9b1), 2026-11-19
- What: slates-vcs `derived::ancestry` (crates/vcs/src/derived/ancestry.rs, tests in derived/ancestry/tests.rs):
  Mononoke's commit-graph fields as one A-647 type `slates-vcs ancestry 1` (generation, first-parent and skip-tree skew
  jumps by Myers' PUSH carried by content links, run bottom, skip-tree parent = immediate dominator); is_ancestor
  (Mononoke lowering), merge_bases (git paint-down with both exits over runs joined as virtual trees, skip-tree jumps
  only to an ancestor at or above EVERY pending entry's generation), bisect (segments + bitsets, exact git rule).
  Research: docs/wip/research/ancestry-queries.md. Docs: amendment "A-666 part 1" at the log's end, A-645's "Queries by
  API" bullet, §4.17.1 Reads status, GAPS' A-645 line.
- Evidence (Windows x86_64, jupiter, main's order while the Studio was offline; macOS owed): red ecb0c2d54 (branch
  agents/g302-a666-red2): 47 passed, 7 failed (all new but the reader's); first green 33f58f903 found two faults (the
  jump guard answered the root for merge bases; bisect's bound miscounted seeds), fixed at 1f4562a0d: 54 of 54 (33.56 s);
  census 40,631 versions, 209 merges, 18 criss-cross bases, 218 jumps, 1,000 walks saved four times over, most visits
  452; walking mutation 2b20fc969 (agents/g302-a666-walk): 51 passed, 3 failed (Myers, long history 195,114 visits vs
  107, oracle). Merge: main touched no slates-vcs since 018d32ba2, so the run stands for the merged tree.
- Commits on the branch: 53ea3710a red (did not compile: 17 E0502 in tests), 0c9ed6490 green, 19ce96f48 refinement,
  c7b9b2cc6 fmt+doc lines, 33f58f903 borrow fix, 1f4562a0d jump guard + bisect bound, 0cdeb8d3e docs, 0e755c9b1 merge.
- Run request filed (run-requests.md, g302 0e755c9b1): the macOS run, clippy, xtask check, doc gate, lint matrix.
- Jupiter recipe: clone with `git -c http.version=HTTP/1.1 -c http.sslBackend=openssl -c core.autocrlf=false clone ...`
  (TLS resets otherwise, g268's tip; my walk clone failed twice before it), CARGO_BUILD_JOBS=4,
  AWS_LC_SYS_PREBUILT_NASM=1, own CARGO_TARGET_DIR, delete after. All my clones, targets and logs there deleted.
- LEFT (A-666's later parts, main starts them): path history and blame; the server's verbs over the type (each
  repository's values on its owner shard, A-662) and a Graph over each tool's commits (git store, hg index) so stock
  clients' ids are the versions; corrected commit dates where a history has them; a green's versions numbered as
  segmented changelog numbers them; questions cut into the shard's slices.
- Lesson: the marker grep `'^<<<<<<< \|^=======$\|^>>>>>>> '` misses `=======` with macOS grep (g307 found it too);
  use `command grep -rnE '^(<<<<<<< |=======$|>>>>>>> )' docs crates`.

## (old) Next piece: A-666 — was REGISTERED to g302, NOT STARTED (ended for context; main relaunches fresh)
- A-645 "Queries by API" part 1: each version's generation number and a skip list over its ancestors (or the
  structure the literature gives the best bound for; cite it) as derived types under A-647 (slates-vcs `derived`);
  merge-base(a, b), is-ancestor(a, b) and bisect's midpoint with no walk proportional to history; oracle: a naive walk
  over generated DAGs with merges compared at every query; a census of query costs held to a cost bound so a regression
  to walking fails. Path history and blame are later parts (main starts them).
- Starting points for the fresh run: `crates/vcs/src/derived.rs` (`DerivedType`, `Graph`, `Fill`; a type's derive gets
  its parents' values in parent order and the store), `crates/vcs/src/derived/tests.rs` (the census runner pattern:
  `TestRunner` through `slates_test_seeds::unseeded`, `RefCell<Census>`, `#[allow(clippy::print_stderr, reason)]`),
  the A-647 entries at the end of docs/wip/SLATES_DESIGN.md. Note for skip lists: a skew-binary jump pointer reads the
  parent's jump's jump, not only the parent's value, so either carry what it needs in the value or state the
  dependency; and general-DAG merge bases have no constant-cost exact answer in general, so the literature and the
  bound claimed need care (research first).

## Side branches (main's correction, 2026-11-19): KEEP agents/g302-a666-red (53ea3710a), -red2 (ecb0c2d54), -walk (2b20fc969)
- The amendment cites their commits. I deleted them on main's first message, then re-pushed all three from my local
  objects on the correction (verified with ls-remote). Delete only side branches nothing cites.

## Next: A-666 parts 2 and 3 — BRIEFED by main, NOT STARTED (ended for context after part 1; main relaunches fresh)
Build on jupiter (Windows x86_64, runner f766f09294f2e17e3b8a631ffd4e6a4b, MSYS bash, sync false) until main says the
Studio is back: clone with `git -c http.version=HTTP/1.1 -c http.sslBackend=openssl -c core.autocrlf=false clone -q
--depth 1 --branch <b> https://github.com/hyper-light/slates.git $HOME/.cache/g302-<what>`, then `git -C <dir> config
core.autocrlf false`; env CARGO_TARGET_DIR=$HOME/.cache/g302-target-<red|green> CARGO_BUILD_JOBS=4
AWS_LC_SYS_PREBUILT_NASM=1; one cargo at a time; delete clone, target and log after reading; evidence says "Windows
x86_64, jupiter" and files the macOS run as owed. Each part lands alone; red first (stubs refusing), green, one
mutation that the bound census must catch. The red and mutation branches are evidence: keep them.
- Part 2, path history, a derived type under A-647: each path's versions that changed it, for `log <path>` and
  `--follow`; renames and copies from the works' own rename operations, never inferred by content. Each query bound by
  the changes to that path, not by history length, proven by a cost census (as part 1's). Oracle: a walk over generated
  histories with renames and copies.
  Starting points:
  - Mononoke's unodes and fastlog: eden/mononoke/derived_data/unodes (a per-path Merkle DAG, a new unode only where
    the path's content changes, its parents the path's unodes in the parents' trees, copy_from from the changeset's
    copy info) and derived_data/fastlog (batches of a path's earlier changesets, so `log <path>` costs its changes).
    Read and cite them, pinned at Sapling 573994010efaffe5a2b4a9fd2a87499ed7caaaf3 as part 1 did.
  - git's history simplification (Documentation/rev-list-options.adoc, "History Simplification", TREESAME) decides what
    `log <path>` lists across merges; state which mode the type answers.
  - The change type: paths.rs's Change (path to Option<Entry>) has no renames. Part 2 needs a source trait carrying
    each path's copy/rename source, taken from the works' ops (crates/merge/src/ops_doc.rs Rename and copies, §4.16
    "Data model"). Read §4.16 before designing.
  - Reuse part 1's machinery where it fits: derived nodes naming their predecessors (content links), Walk-style
    arena and charging, the census pattern, `drive`/Fixture in derived/ancestry/tests.rs.
- Part 3, blame, a derived type: each line's introducing version, built from the parent's value and the version's
  change, with no full-file diff of history. Read and cite Mercurial's linelog (mercurial/linelog.py in 7.2.4, the
  tarball part 1's research pinned; hg's annotate with linelog: hgext/fastannotate) and Mononoke's blame
  (eden/mononoke/derived_data/blame: BlameV2 per file unode, from the parents' blames and one diff with each parent's
  content).
  - Oracle: blame recomputed from scratch by replaying every version.
  - D-27 bans inferring a merge, so a line both parents changed is attributed by the merge's own recorded result: a
    line the merge's content shares with a parent's (by the diff with that parent) keeps that parent's attribution, one
    in neither is the merge's own. Say so in the design and test it (both parents changing one line).
- The server's query verbs over these types come after A-662 lands; main starts them.
- Part 1's lessons for 2 and 3: (1) any jump or skip past a region must be guarded by EVERY pending entry's reach,
  not only those below; the oracle found it at once. (2) A stated bound must count every seed and every visit kind;
  the first green found a miscount. (3) In tests, take roots before lending the store (`&mut fixture.store` and
  `fixture.root(..)` in one call is E0502). (4) Use the ERE marker grep after merges.

## A-666 part 2 (path history) — LANDED 6651ed8d6 (main 40bff1d55..6651ed8d6), 2026-11-19 (relaunched run)
- What: slates-vcs `derived::history` (crates/vcs/src/derived/history.rs, tests history/tests.rs), type
  `slates-vcs path history 1 <window>`: value = [path map, history tree]; history tree = a path map of each path's node
  (Mononoke unode) at `p/\0` (root `\0`; NUL names refused ReservedName). Rule: keep the node of the FIRST parent the
  path is TREESAME to (git default mode exactly, revision.c 1001-1003/1039-1066), else make a node (parents' nodes in
  order, deduped; removal = node of no file; source node for recorded renames/copies). Directory TREESAME = its trie
  root in the path map (files), NOT children's unodes (Mononoke v2 differs; tested). `log(store, value, path, follow,
  limit)` newest first (generation, name, path). Mechanism: `DerivedType::inputs` (parents by default; history adds copy
  source versions); `paths::at` (what stands at a path + nodes read). Research: docs/wip/research/path-history.md.
- Evidence (Windows x86_64, jupiter; macOS owed): red b05e17991 (agents/g302-a666p2-red, KEEP): 55 passed 10 failed;
  first green 408bb0c91 65/65 112.68 s (unused Graph import; census bound used the walk's own count -> fixed);
  green 4263443d6 65/65 104.41 s; census 1,648 versions, 174 merges, 19 octopus, 54 from parent, 30 from older,
  4,311 nodes, 342 removals, 124 sourced, 291 diverted, 770 questions (447 empty), 72 cut, 136 walks saved, max 79
  reads; long history 20,000 versions: log hot 1,001 reads, cold 7, cut-10 12. Mutation f0cc723f7
  (agents/g302-a666p2-walk, KEEP): list whole then cut -> 63 passed 2 failed (122 reads vs bound 10).
- Merge: main's 31 commits touched nothing of slates-vcs/manifests/toolchain; one conflict (amendment log end) kept
  whole, main's A-659, A-664 p1 record, A-667, A-636 p1 then A-666 p2.
- Run request owed: macOS run, clippy, xtask check, doc gate, lint matrix (filed in run-requests.md).
- Lesson: on jupiter a failed clone left the previous run's log, and the grep read it as this run's: always
  `rm -rf $L` first and retry the clone up to 3 times; check the printed commit id is the one pushed.

## Next: A-666 part 3 (blame) — STARTED in the same run (see below)

## A-666 part 3 (blame) — IN PROGRESS (relaunched 2026-11-19 ~07:00 after the disk filled at 05:13)
- Worktree ~/slates-work/g302-a666p3 (of g302-base), branch agents/g302-a666-blame (tracks origin/main: push ONLY
  with explicit refspecs). Base 6651ed8d6. Scratch ~/slates-work/g302-scratch/a666p3 (blame.green.rs, blame.red.rs,
  blame.tree.rs = the mutation, msg-*.txt); sources ~/slates-work/g302-scratch/a666p3-src (hg 7.2.4 linelog +
  fastannotate, sapling blame_v2.rs, dmp diff_match_patch.py @62f2e689, git-blame.adoc @e1ec09b2, myers diff2.pdf).
- Commits (pushed): red 160b47ebb (agents/g302-a666p3-red, KEEP; derive, blame, matched stubbed) and green d7acb9700
  (agents/g302-a666-blame), siblings off 6651ed8d6. Expected: red 66 passed 7 failed (all new but hostile bytes);
  green 73/73 (65 + 8 new).
- Mutation planned (blame.tree.rs): every version re-blames every file the first parent holds that the change leaves
  (same blame made again): answers and identities unchanged; expected 72 passed, 1 failed (the oracle's
  (judged, lines_read) check). Branch agents/g302-a666p3-tree, parent = green tip.
- Docs: research/path-history.md §6 drafted (uncommitted in the worktree; add Measured + Rejected after runs);
  blame.rs cites fastannotate __init__.py 19-22 but the quote is 23-26: fix in the docs commit. Then the amendment
  entry, A-645 "Queries by API", §4.17.1 "Reads" status, GAPS line 6; merge origin/main; push to main.
- Jupiter: clones from GitHub reset often (use 10 attempts, 30 s apart); kiln restarted at ~07:40 (old run ids gone);
  fmt per crate only (`cargo fmt -p slates-vcs -- --check`; --all hits os error 206).

## A-666 part 3 — relaunch 2026-11-19 ~16:20Z (fresh g302): state
- Readback of THE RULE sent to main. Reviewed d7acb9700 (code never compiled before this relaunch).
- Added two tests (green 78dfa5ad5 on agents/g302-a666-blame, pushed): `..._a_line_both_parents_hold_is_the_first_parents`
  (the D-27 test never had a line both parents' diffs keep) and `..._forged_values_are_refused_as_malformed` (class guard,
  bare content, span bytes past the object, a parent's blame counting too few lines: none had a test).
- Red: db61572b3 on agents/g302-a666p3-red (160b47ebb + the same two tests), pushed. Expected 66 passed, 9 failed.
- Mutants: agents/g302-a666p3-mutants (pushed), 13 commits, each = green 78dfa5ad5 + one hunk in blame.rs:
  m1 c840e2698 tree/bound (oracle counters) | m2 46a480a4d last parent wins (first-parent test, oracle) |
  m3 da000a275 later parent's same content not kept (oracle) | m4 2bb8ac4fa line-count guard (forged) |
  m5 f55b1e442 class guard (forged) | m6 cf954b070 lines==0 (hostile way 0) | m7 22d73ae3b no version (way 1) |
  m8 ae45a39d1 no path (way 2) | m9 d2d5e198f joins (way 3) | m10 46c1815e7 total (way 4) | m11 df4a395ac third name |
  m12 fdd4bc605 tag | m13 f8d567e0f payload cut short.
- Jupiter run r50 (red, green+fmt+census, 13 mutants on `derived::blame`), logs on jupiter ~/.cache/g302-p3-logs (delete
  after reading). Started ~16:40Z.
- Uncommitted in ~/slates-work/g302-a666p3: research §6 draft; blame.rs comment-only fixes (fastannotate 23-26,
  blame_v2 514-565 paraphrased, the `whole` check's comment). All citations checked against the copies: hashes match §1.
- Next: read r50; amendment (scratch amend-p3.md, fill @@TESTS@@), A-645 bullet (~44261), §4.17.1 status (~12261),
  GAPS line 6 (add Part 3 BUILT, drop blame from OWED), research §6 Measured/Rejected; commit docs; merge origin/main
  (conflict expected only at the amendment log's end: main's entries first); marker grep; diff --stat; push HEAD:main.
- Side worktrees: ~/slates-work/g302-a666p3-red, ~/slates-work/g302-a666p3-mut (scratch; branches are evidence: KEEP).

## A-666 part 3 — relaunch 2026-11-19 ~17:00Z (fresh g302, second relaunch)
- Committed the citation fixes: ee6868011 on agents/g302-a666-blame (pushed; comments only). Research §6 draft still
  uncommitted in ~/slates-work/g302-a666p3 (goes with the docs commit).
- Main e3234e618: its 13 commits since 6651ed8d6 touch nothing of crates/vcs, manifests, lock, toolchain, test-seeds.
- jupiter run r87 (red db61572b3, green ee6868011 + fmt + census, 13 mutants on derived::blame), one target
  ~/.cache/g302-target, logs ~/.cache/g302-p3-logs (delete both after reading). Started ~17:00Z.

## A-666 part 3 (blame) — LANDED bf9d39139 (main e417bfc72..bf9d39139), 2026-11-19 ~18:50Z
- Commits on main: d7acb9700 the type (crates/vcs/src/derived/blame.rs, blame/tests.rs; derived.rs docs; history.rs
  `inputs_of` pub(super)), 78dfa5ad5 two tests, ee6868011 citations (comments only), d15a030f0 merge e3234e618,
  beee288c8 docs (amendment "A-666 part 3" at the log's end, A-645 bullet, §4.17.1 Reads status, GAPS line 6, research
  path-history.md §6), bf9d39139 merge e417bfc72 (one conflict, the log's end: main's A-656, A-656 cont. (1), A-616
  cont. (5), A-668 kept whole, then A-666 part 3; design doc checked to differ from main only by my three changes).
- Evidence (Windows x86_64, jupiter, kiln r87, `cargo test -p slates-vcs --lib`): red db61572b3 66 passed 9 failed
  (all new but hostile bytes), built 15m51s, tests 255.83 s; green ee6868011 75/75, 213.05 s, built 3m22s, fmt clean;
  blame census versions 818, merges 215, renames 49, copies_from_older 28, blames 2299, kept 153, made 708, merged 74,
  merge_own 103, later 175, through_source 259, lines 6963, most_lines 12. slates-vcs and its build inputs identical
  between ee6868011 and bf9d39139 (main touched none).
- Main's §8 message mid-run: dropped the 13 mutation red-checks (r87 stopped right after cloning the mutants; none ran)
  and no reruns. Mutation run filed for main in run-requests.md with each mutant's expected failing test.
- Findings (to main): `BlameCost` (the question's cost) is asserted by no test (recorded as owed); the oracle's
  MAX_VERSIONS = 160 cap is never met (ops are 1..64, so at most 127 versions a case; the amendment says so).
- jupiter: my clones, target ~/.cache/g302-target and logs deleted (verified "no g302 files left").
- Run request filed: 2026-11-19T18:55Z g302 main bf9d39139 (macOS run, clippy/xtask/doc gate, the mutation run).
- Branches KEEP (cited): agents/g302-a666p3-red (db61572b3), agents/g302-a666p3-mutants (f8d567e0f).
- Worktrees ~/slates-work/g302-a666p3, -red, -mut: scratch, removable once main confirms.
- A-666 is complete (parts 1-3). LEFT (main starts them): the server's verbs over A-666's types (A-662), a
  `PathSources` over each green's versions, git's other simplification modes, blame's spans shared by identity.

## A-647 part 3 — git's store serves D(k) and reached sets from its ReachSets values (main's brief 2026-11-19 ~19:20Z)
- Brief: "the next owed item of A-647, git's store reading its derived data"; failing test first; focused unit +
  integration tests + fmt on the Mac Studio (d1323898b255a3ed435eb83fa0f7b0cf, sync false, CARGO_BUILD_JOBS=2,
  CARGO_TARGET_DIR=~/.cache/g302-target, delete at END); land as one commit plus the merge; END. BlameCost's test is
  NOT this piece (slates-vcs blame, not git's store): stays owed.
- Worktree ~/slates-work/g302-a647p3, branch agents/g302-a647-serve, from origin/main effb30ac2.
- Design (decided):
  - ReachSets format 2 ("slates-git reach sets 2"): a derived node naming ONE object (the sets' bytes, put_object, any
    length; format 1's payload layout) with payload = the object's length (8 LE). Reason: put_derived refuses a node
    past one window (git tests WINDOW 1 KiB, server tests 256 B), so format 1 holds ~14-60 runs; today keep_values
    silently skips such values; a store serving from values would refuse the commit. Blame's spans object is the model.
  - Repository: Version { head, value: ContentId (held by the record), fragments, followed } (no `brought`); Chain
    without `reached`. commit: plan -> put each move's value (released on any later refusal) -> footprint charge ->
    rows. brought(store, green, version) / reached(store, green, version) -> Result<Positions, StoreRefused> decoded from
    the version's value (any version's reached set now answerable); new StoreRefused::SetsUnreadable{green,version}.
    admit/remade/protect/encode_fragments/rebuild_base decode once; graph.rs records GraphFault::Store on a refusal;
    upload.rs decodes a version's D(k) once and keeps it while gathering that version (not per position).
    reach_value returns the newest version's own value + store.hold for the caller (server keep_values unchanged).
    release() gives back every version's value.
  - Tests: A-614 oracle check_green holds EVERY version's reached set to model.reach(head) (red: today's store keeps only
    the newest); reach oracle the same; focused scenario (ff, rewrite, delete, re-create); a value past one window
    (two greens interleaved so a reached set has more runs than a window holds); hostile values in format 2.
  - Red = the new API over today's chains (older versions refused NoVersion), a side branch not for main.
- State ~20:45Z: red a9c2adfa2 (agents/g302-a647p3-red, KEEP) r138: 81 passed 5 failed (the 5 new/extended).
  Green 32bfcf364 + docs a9746ee6d on agents/g302-a647-serve (r155: git filters 185/185 4 ign, fmt clean, server
  vcs_store 13/13; r167: server vcs_http::git + repository::push 14/14).
  Main moved to 1ef718820: it moved fragment/base encoding into store/encoding.rs (unencoded, head_reach helpers).
  Squashed onto it in ~/slates-work/g302-a647p3-land (branch agents/g302-a647p3): conflicts resolved (repository.rs
  helpers read values, Encoding::base takes the store, its test adapted; GAPS line 6 = main's + my clause; design log
  end main's first). Test commit 9c01b9181 pushed to agents/g302-a647p3-merged; run r177 on it.
- Next: if r177 green -> fresh worktree from origin/main, `git merge --squash agents/g302-a647p3` (tree of 9c01b9181),
  update the amendment's Tests with r177's numbers, commit ONCE, diff stat, markers, push HEAD:main. Then delete the
  Studio target ~/.cache/g302-target and logs ~/.cache/g302-logs; run request; report; END.

## A-647 part 3 — LANDED b78296d4c (main 901be4f52..b78296d4c), 2026-11-19 ~20:45Z
- One commit d8111a795 (git: the store serves every version's D(k) and reached set from its ReachSets value, chains'
  copies dropped, format 2) + merges f2104bff8, bf5c7bd38, b78296d4c (main moved three times while landing; each
  log-end conflict kept main's entries first; GAPS line 6 = main's text + my clause, owed item removed).
- Evidence (macOS arm64, Mac Studio): red a9c2adfa2 r138 81 passed 5 failed (the 5 new/extended); green 32bfcf364 r155
  git filters 185/185 + server vcs_store 13, r167 server git/push 14; merged tree 9c01b9181 (main 1ef718820, encoding
  moved to store/encoding.rs) r177 190/190 + server 29; final d8111a795 on c3c2c045f r194 190/190 + server 40 (with
  vcs_ssh::git); merged head bf5c7bd38 r214 192/192 (main's delta tests) + server 40; fmt clean every run.
- Studio target and logs deleted (verified). Run request filed (2026-11-19T21:00Z g302 main b78296d4c): Linux and
  Windows runs, clippy/xtask/doc gate. Branches KEEP (cited): agents/g302-a647p3-red, agents/g302-a647-serve,
  agents/g302-a647p3-merged, agents/g302-a647p3-final.
- Findings for main: each version's value repeats its whole reached set (part 2's design; owner's index kept them
  already): window test 2,145 runs vs the chains' 130; sharing a parent's runs by identity owed. BlameCost's test
  (A-666 part 3) still owed (not this piece).
- Worktrees (scratch, removable): ~/slates-work/g302-a647p3, -land, -final; the A-666 ones.
- LEFT for A-647 (main starts): values sharing runs between versions; dropping values no version needs; A-553's and
  A-588's stores as types.

## A-647 part 4 — ReachSets values share their parent's runs (main's brief ~21:00Z): O(added) stored a version
- Brief: failing test first (a branch pushed by turns stores O(added) runs per version, not O(reached)); focused
  tests (git store, reach, oracle) + fmt on the Studio; one commit + merge; END. BlameCost's test is a later piece.
- Design (decided): format 3 "slates-git reach sets 3 <window>": each set a canonical radix tree over fixed blocks of
  positions; leaf = derived node, runs RELATIVE to its block (so equal blocks are one node by identity); inner = node
  naming its present children, payload level + bitmap; a fully covered subtree is one node per level ("full(l)").
  Block width B = 2 * floor((W - leaf overhead)/16) positions, fan-out F = most names a node of W holds (both computed
  with slates_vcs's encode_derived, so derived from the store's format). Root level = least covering the max block.
  Built from scratch each version: the store's content addressing stores only new nodes (storage O(added)); CPU
  O(reached) as format 2 already hashed. Value node names [reached root?, brought root?], payload tag + presence.
  Path map rejected: its floor (one full 16-way branch) exceeds the server tests' 256-byte window, chosen so blobs span
  windows. ReachSets::declare(window, mode); GitStore::new takes the window. Counters: fresh value nodes/bytes.

## A-647 part 4 — STOPPED for main's redirect (2026-11-19 ~21:50Z), OWED; branches KEEP
- Relaunched g302 found the predecessor's red bbd4bad12 (agents/g302-a647p4-red; NEVER RUN) and an uncommitted,
  non-compiling green in ~/slates-work/g302-a647p4 (local branch agents/g302-a647p4). Saved: 65382d944 (as left) and
  1067842cb (completed, NEVER COMPILED) on agents/g302-a647p4-wip. Patch backup:
  ~/slates-work/g302-scratch/a647p4-uncommitted-20261119T212426Z.patch; final storage test ~/slates-work/g302-scratch/a647p4/storage-test.rs.
- Finding (by reading content.rs, not yet measured): the content store cuts an object at fixed offsets 0, w, 2w and keeps
  each window once, so format 2's value for a version that APPENDS a run (a branch pushed by turns) adds only the first
  window (the count), the last ones and the lists on two paths: O(log) windows, not O(reached). Format 2 stores O(reached)
  only when D(k) lands BELOW its newest run (a merge of older work): every window after it moves. So the red must include
  the insert-below case; the brief's push-by-turns case alone would pass on format 2 at scale.
- 1067842cb's test: phase 1 400 roots on refs 2/3 by turns; phase 2 ref 1 by turns with ref 3 (appends); phase 3 merges
  of ref 2's roots newest first (inserts below); each of ref 1's 801 versions held to
  (touched(D(k)) x (path(reached) + path(D(k))) + 1) x window, path = level_of(last block) + 1; every version's sets
  checked; census printed. Predicted (by hand, W=256 -> shape block 30, fanout 7, 3 levels): format 2 late merges ~13 KB
  vs bound 4,352 (red); format 3 ~0.8 KB a turn, ~1.8 KB a merge.
- Resume: (1) red: worktree off agents/g302-a647p4-red, replace its storage test with storage-test.rs (it compiles
  against the red's Shape), commit on the red branch, run `cargo test --locked -p slates-git --lib -- t_a647` on the
  Studio: expect only t_a647_p4_... failing; (2) green: run 1067842cb (fix compile errors), part 3's focused commands
  (`-p slates-git --lib -- store served protected receive object_info upload`; `-p slates-server --lib -- vcs_store
  vcs_http::git vcs_ssh::git repository::push --test-threads=4`), fmt; (3) docs: amendment "A-647 part 4", §4.17.2
  status, A-645 bullet, GAPS line 6, research note on the format-2 window finding; (4) squash-land from origin/main.
- Design notes to keep: the fanout/block are the most a window holds; with macOS's 256 KiB window a dense inner node is
  up to a window, so a version on a large repo may add up to (depth x window); a smaller derived node size (fewer bytes
  a version, more nodes) is a measurement owed, not a guess. Sets::read trusts server-made nodes (no cap on expansion;
  nodes cannot be forged by clients: key-derived identity context).

## NEXT (main's redirect 2026-11-19 ~21:50Z): git receive-pack over SSH — IN PROGRESS
- Brief: the SSH site's `git-receive-pack '<repo>'` calls g258's ReceivePack driver (main 0577ab0bd: `start(&mut
  ShardState, (&Principal, RepositoryId), (room, advertise), &mut Out)`, then input, end, answer), so a stock `git push`
  over SSH lands through it. g258 wires the same driver into HTTP; g294 lands SSH key lookup: merge often.
  Failing test first (vcs_ssh's tests already exec git-receive-pack; make a scripted push land); focused tests (vcs_ssh,
  git_push) + fmt on the Studio; one commit + merge; END.

## A-683 (git receive-pack over SSH) — relaunch 2026-11-19 ~22:15Z (fresh g302): state
- Predecessor left: red 954ed792e (agents/g302-ssh-receive-red, KEEP: cited; Studio red.log 28 passed 2 failed), green
  7f2c9c791 + docs 99b909d7a (agents/g302-ssh-receive, pushed; Studio green.log 30/30), and an unfinished merge of main
  3b0b95379 in ~/slates-work/g302-ssh-receive (left as is: scratch; never aborted).
- Main's brief: mirror f05af5288's HTTP shape (ShardLender/ThisShard lending with_state a step at a time). Done:
  ReceiveCommand<S: ShardLender>; ToolServices<L, S>::new(store_lender, shard_lender, room); ToolCommand<L, S>;
  vcs_http::git re-exports ShardLender; two refusal tests (NotLent -> not served 128; repository gone -> git's words);
  test harness `ended: true`; NoPacks::write takes the admission (main's e4c6e7cbf).
- Tested tree a00f47d7c on agents/g302-ssh-receive-land (side branch, not for main), over main b94ae6e90; worktree
  ~/slates-work/g302-ssh-land. Studio run r286: fmt + `--lib -- vcs_ssh git_push vcs_http --test-threads=4`, logs
  ~/.cache/g302-logs/land*.log, target ~/.cache/g302-target, clone ~/.cache/g302-ssh-land (delete all at END).
- Docs placeholders to fill from r286: SLATES_DESIGN.md A-683 entry @@RUN@@ / @@COUNTS@@; GAPS A-683 @@GAPS-RUN@@.
- Landing: worktree ~/slates-work/g302-a683 (branch agents/g302-a683 from origin/main): `git merge --squash
  agents/g302-ssh-receive-land`, fill placeholders, one commit, marker grep, diff --stat, push HEAD:main.
- ~22:55-23:15Z: r286 (a00f47d7c over b94ae6e90) 74/74; main moved: side merges 0041eb501 (770ae8826) r313 77/77,
  288566d81 (693bc8feb) r354 82/82, 002fe9d0a (2f73eb075 = A-651 part 4, the SSH move) r375 running. Landing branch
  agents/g302-a683: piece commit 4681b17b6 (pushed to the side ref agents/g302-a683), local merge 93621c213 (693bc8feb),
  merge of 2f73eb075 staged (tree 05655bfc9 = side 002fe9d0a). A-651 part 4 closed the "move" owed item: docs updated
  (receive.rs "Where", §4.17.2 status, amendment Owed, GAPS Owed: a push over a moved connection, two shards).
- Next: fill r375's count into the amendment's Evidence and GAPS; commit the merge (message with r375); fetch; if main
  moved again judge/merge; diff --stat; markers; push HEAD:main; run request; report; delete Studio clone/target/logs.
- ~23:30Z-00:15Z: main kept moving; merged into agents/g302-a683 (local): 93621c213 (693bc8feb), 98431ff46
  (2f73eb075, A-651 part 4), 3eb422d67 (f1fbd2c44), 5fc4529f5 (f7684fc20), 3d3667323 (5206419be, A-682: ToolServices
  calls adapted, receive test over ShardStores; vcs_store/upload/tests.rs adapted), now merging be892650b (A-679:
  ToolServices<L, H, S>::new(lender, hg, shard, room), ToolCommand Upload/Receive(boxed)/Mercurial; tests/hg.rs
  adapted; vcs_http daemon_edge back to private, test uses vcs_http::ShardRepositories). Runs: r403 85/85, r434 build
  refused (A-682's test, E0061), r454 92/92, r482 running on side 2f5abc24f (tree 6cfd3e50c = landing index).
- Removed my landed worktrees (a647*, a666*; a666p3-mut's one-file diff saved in g302-scratch/a666p3). Disk 7 -> 31 GiB.

## A-683 — LANDED a7b5e4292 (main e21beb606..a7b5e4292), 2026-11-20 ~00:35Z
- Piece commit 4681b17b6 (server: git's SSH site serves receive-pack through the push driver, each step lent the owner
  shard's state through the HTTP site's ShardLender) + merges of main as it moved: 93621c213, 98431ff46, 3eb422d67,
  5fc4529f5, 3d3667323, 734548412, b25fc2fd0, 149180f2d, a7b5e4292 (main's A-651 p4, A-664 p3, A-689, A-682, A-679,
  A-665 p3, A-688, A-684, A-686 adapted to: ToolServices<L, H, S>::new(lender, hg, shard, room); ToolCommand
  Upload/Receive(boxed)/Mercurial; answer_step loop with Unpresented; daemon's SSH site serving receive-pack).
- Evidence (macOS arm64, Mac Studio): red 954ed792e r163 28 passed 2 failed; r286 74/74 ... r530 124/124 over the
  landed code (`-p slates-server --lib -- vcs_ssh git_push vcs_http vcs_store::upload --test-threads=4`), fmt clean.
- Branches KEEP (cited): agents/g302-ssh-receive-red (954ed792e), agents/g302-ssh-receive (99b909d7a, first draft),
  agents/g302-ssh-receive-land (side branch of tested merges, a61f5282e). agents/g302-a683 (4681b17b6) is on main.
- Cleaned: Studio ~/.cache/g302-* deleted (verified); local worktrees g302-a683, -ssh-land, -ssh-receive removed (the
  predecessor's unfinished merge saved: g302-scratch/ssh-receive/predecessor-unfinished-merge-20261120.patch).
  ~/slates-work/g302-base (clone) kept.
- Run request filed 2026-11-20T00:35Z. Findings for main: git_interop's stock_git_clones_pushes_and_fetches_over_the_ssh_edge
  can be un-ignored now (A-684 + A-683 landed); a push over a moved connection (two shards) has no test yet.

## NEXT (main's brief 2026-11-20 ~00:40Z): Subversion finish line — inventory, then the first missing piece
- 1. Inventory every §4.17.4 / GAPS owed Subversion piece + every svnserve and mod_dav_svn feature (ra_svn commands,
  DAV methods and REPORTs, locks, revprops, mergeinfo, replay/svnsync, svnrdump ranges, authz, hooks, commit editor,
  import/export, protocol versions and capabilities): built / in flight (owner) / missing, one line each, to main.
  In flight (don't rebuild): SvnStores lender + DAV REPORTs/update-report/routing + DAV writes (g297); svn:// lookup and
  connection move (g234); ra_svn commits through svn_commit (g229); CRAM-MD5 + ra_svn lock verbs (g307); A-652 export
  (g268).
- 2. Build the first missing piece by user impact: claim an A-number in amendments.tsv, failing test first, one commit +
  merge; focused unit + integration tests + fmt on the Studio; 3-line report; go on; END when context runs short.

## Subversion inventory — SENT to main 2026-11-20 ~00:45Z (reading-based; no route reaches a repo by use yet)
- MISSING (no owner): ra_svn change-rev-prop/change-rev-prop2 (+ pre/post-revprop-change calls; hook points exist in
  db catalog A-648) = A-702 (claimed by g302); ra_svn replay/replay-range (svnsync source, svnrdump dump; the replay
  drive exists for DAV, A-627); DeltaV v1 (MKACTIVITY/CHECKOUT); Cyrus SASL mechanisms (optional); review (piece 14).
- FINDING sent: the greeting advertises atomic-revprops and partial-replay while change-rev-prop2 and replay are
  unserved ("Unknown editor command"), which misleads svnsync.
- In flight per main: g297 (SvnStores lender, DAV REPORTs/routing, DAV writes incl. PROPPATCH revprops), g234 (svn://
  lookup + move), g229 (ra_svn commits through svn_commit), g307 (CRAM-MD5 + ra_svn lock verbs), g268 (A-652 export).

## A-702 — CLAIMED, NOT STARTED (g302 ended for context after A-683 + the inventory). Plan for the next g302:
- Stock rules to transcribe (Subversion 1.14.x d434dc14 / 1.15.0, the copy the svn crate cites):
  serve.c `change_rev_prop` ("rc?s") and `change_rev_prop2` ("rc(?s)(b?s)") -> `do_change_rev_prop`:
  `must_have_access(..., svn_authz_write, NULL, FALSE)`, then `svn_repos_fs_change_rev_prop4(repos, rev, author, name,
  old_value_p, value, TRUE, TRUE, authz_read_func, ...)`, answered with an empty success. repos/fs-wrap.c
  `svn_repos_fs_change_rev_prop4`: revision readable whole (SVN_ERR_AUTHZ_UNREADABLE "Write denied: not authorized to
  read all of revision %ld"), `svn_repos__validate_prop`, the action A/M/D from the old value, pre-revprop-change
  (no hook -> SVN_ERR_REPOS_DISABLED_FEATURE "Repository has not been enabled to accept revision propchanges;\nask the
  administrator to create a pre-revprop-change hook"), `svn_fs_change_rev_prop2` with the expected old value
  (SVN_ERR_FS_PROP_BASEVALUE_MISMATCH "revprop '%s' has unexpected value in filesystem"), post-revprop-change.
- Where: slates-svn `serve.rs` (drop the two names from reads.rs ELSEWHERE's unserved set; a Turn for the caller like
  commit's Hooked/Accepted: run pre-revprop-change through A-661's svn_hooks glue, apply A-655's
  `Record::RevisionProperties { revision, properties }` through the owner's step — `crates/server/src/repository/
  svn_commit.rs` already applies that record and recovery replays it — then post-revprop-change, then answer). The
  daemon's svn:// connection task is g229/g234's area: merge often, coordinate.
- Failing test first: recorded stock svnserve 1.15 sessions (find the svn crate's capture script for ra_svn sessions):
  `svn propset --revprop -r1 svn:log` with an allowing hook, the same with no hook (165006), change-rev-prop2 with a
  stale old value (svnsync's sync-lock; 160049); replay each against the connection.

## A-702 — relaunch 2026-11-20 ~01:40Z (fresh g302): STARTED
- Read relaunch-2026-11-19.md (completeness first), restart preamble, CLAUDE.md §8 (origin/main), lint-habits.
- main a7b5e4292 has no A-702. Worktree ~/slates-work/g302-a702 (branch agents/g302-a702 from origin/main a7b5e4292).

## A-702 — relaunch 2026-11-20 ~02:10Z (fresh g302, after the 17:30-18:00 PST stop): state
- Predecessor left only the worktree ~/slates-work/g302-a702 (clean, at a7b5e4292); no commits, no kiln run.
- origin/main 3b392dcb1: no A-702. Claimed A-703 (ra_svn replay + replay-range) in amendments.tsv (next free A-704).
