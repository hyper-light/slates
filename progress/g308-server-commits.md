# g308 — A-664: the commits the server makes (UI goal 93, kiln 51cef9f7)

Supervisor: main. No build on this MacBook (cargo fmt only). Builds on the Mac Studio (d1323898, `sync: false`, a
GitHub clone, CARGO_TARGET_DIR ~/.cache/g308-target, CARGO_BUILD_JOBS=2). Nothing of g308's is left on the Studio
(clones, logs and the target removed again 2026-11-19 ~23:15Z, kiln r386; local land/main worktrees removed, all pushed) or on jupiter.
## RELAUNCHED (2026-11-20 ~04:00Z, fresh g308): landing A-664 part 3's commit
- Found: the Studio run over ecc01fff4 (piece + main 3b392dcb1; logs ~/.cache/g308-logs-p3i) GREEN: server lib filtered
  215 passed 2 ignored, fmt clean, edit_and_submit 2/2, daemon 7/7, works_off_owner 5/5, fleet 14/14 (190.5 s).
- g308-p3c had an unfinished merge of main b63f8a05d (A-629 part 3 LFS); resolved (design log's end, main's first),
  committed 09a38d73d, pushed agents/g308-a664p3c. Studio run p3j (logs ~/.cache/g308-logs-p3j, filter + lfs::) over it.
- g308-p3c-land's staged squash (over 20ae700a7) is superseded: land from a fresh worktree off origin/main with
  `git merge --squash agents/g308-a664p3c`, add the runs to the entry's Runs bullet, one commit, push HEAD:main.
- The mode bit: LOST (documented in the entry: one work's chmod+mv refused SetModeMissing; two versions drop it) ->
  the next piece is the mode fix (test in ~/slates-work/g308-scratch/mode-test-for-next-piece.rs).


## LANDING (2026-11-20 ~00:40Z): A-664 part 3's commit -- red r469, green r495; r532 over main 20ae700a7
- Piece branch agents/g308-a664p3c: de4c0ddb5, 8a30b5e8a (mode test moved out), 4fc6b8abc (runs doc), merge
  4f023cf33 of main 20ae700a7 (tree 17abd651a). Land worktree ~/slates-work/g308-p3c-land (branch
  agents/g308-a664p3c-land at main 20ae700a7, squash STAGED, tree 17abd651a == the merge's, NOT committed yet).
- Red r469 (de4c0ddb5 over 2c3fa2bcc, make stubbed): vcs_store::commits:: 0/5 (main unlisted x3, no parent x1, mode
  test submit refused SetModeMissing). Green r495 (8a30b5e8a over 2c3fa2bcc): server lib filtered 193 passed 2 ignored;
  fmt clean; edit_and_submit 2/2, daemon 7/7, works_off_owner 5/5, fleet 14/14 (186.9 s).
- NEXT on r532 green: in g308-p3c-land add r532 to the entry's Runs bullet, git add, ONE commit with the final message,
  fetch, merge origin/main if moved (check conflicts/markers, diff --stat), push HEAD:main; amendments.tsv A-664 row;
  run-requests entry; 3-line report to main; then the NEXT PIECE (mode fix): its failing test kept in
  ~/slates-work/g308-scratch/mode-test-for-next-piece.rs (one work: create d/a, chmod 0755, mkdir x, mv d/a x/a ->
  submit refused BadRequest "increment does not compose: SetModeMissing(\"d/a\")"; two versions: mode dropped).

## EARLIER (2026-11-20 ~00:10Z, relaunched g308): A-664 part 3's commit -- CODE WRITTEN, Studio run r469
- Branch agents/g308-a664p3c (pushed de4c0ddb5 over main 2c3fa2bcc), worktree ~/slates-work/g308-p3c. Landed
  worktrees g308-part3 and g308-server-commits removed (main's disk ask); g308-identities kept (uncommitted draft).
- Decision: NO new entry kind. The register (db repository.rs apply_advanced, entry.rs check) takes an entry holding
  objects alone (no position, never atomic; A-636 part 2b makes no state-log record for it). The made commit is
  Op::RepositoryAdvanced{entry{repository, objects: push-format record}} appended in the submit's transaction after
  GreenAdvanced; replayed by replay_git like a push's record.
- Code: paths.rs keeps Head{version, root, trees} (Trees<Heads> made over the same Change / built whole with the map;
  both or neither); vcs_store.rs GitStore.trees/commits declared in made(); release_head; new vcs_store/commits.rs
  (make -> made: parent_of (newest store head, via MemberOrigin::Branch for a CreateBranch branch), Commits::make,
  Lacked walk of trees the repository lacks + blobs from the map, GitPush admit/seal/admit move/record, check ->
  vcs_store::commit -> mutate, rebuild on append refusal; counters vcs_commits.made/unmade); verbs.rs hook after
  paths::advance. Tests vcs_store/commits/tests.rs (5): clone via A-682's upload tests helpers (made pub(crate)).
- Mode check BY USE = t_a664_p3_an_executable_file_moved_is_100755_in_its_commit: if it FAILS green, move it to the
  next piece (mode fix, docs/bugs record); if it passes, tell main "no repro" in one line.
- Script ~/slates-work/g308-scripts/studio-p3f.sh (red: commits::make stubbed; green: server lib filtered, fmt,
  edit_and_submit, daemon 7, works_off_owner 5, fleet 14), logs ~/.cache/g308-logs-p3f, target ~/.cache/g308-target.
- NEXT: docs (design entry A-664 part 3 its commit, §4.17.1/§4.17.2 statuses, piece 13, GAPS), land squashed.

## EARLIER PLAN (2026-11-19 ~23:20Z): A-664 part 3's commit (main's assignment)
- Goal: git's trees and commit derived from the maps, admitted to the git store with the green's head moved, recorded
  with the version, so a submit through a work becomes a real git commit a stock clone fetches. Failing test first.
- Mode finding BY USE inside this piece's integration test: in a work write a file, chmod +x, mv, submit; read the
  server-made commit's tree (git ls-tree over the store, or a stock clone once A-682 lands). 100644 -> real bug, fix as
  its own piece right after (failing test first, docs/bugs record); no repro -> tell main in one line.
- After: holders' maps, a copy through a work, sliced whole builds, hg and svn objects.
- STATE at ~23:35Z: design read, NO code written, no branch yet (context ran short). Start fresh from origin/main.
- Decision (from A-662's entry, SLATES_DESIGN.md "### A-662", and §4.17.1 "A push is a work" status A-664 part 2):
  the made commit's objects must be RECORDED like a push's (A-662's `vcs_store::record`: moves by green key, the version
  the entry leaves the green at, the commit its head names, every object the repository lacks with its bytes), not only
  derived: a recovery must remake every made commit, and a folded version's tree can no longer be derived. The commit
  bytes carry the message, identity and time, so the owner's replay needs no merge-record field; the holders'
  recomputation (3.3) will need message + identity in the version's record (part 1's owed "merge record carrying them").
- Where: `verbs::judge_submission` right after `crate::vcs_store::paths::advance(state, green_id, version)` (verbs.rs
  ~9549), inside `run_recorded`'s transaction, so the made commit's register entry is one durable atom with the version's
  increment (CLAUDE.md: `crates/db/src/replay.rs` begin..commit). Authorship (message + git identity) is in hand at the
  submit (part 1's `identities`); the time: the merge record's (`merge_service::enqueue_record`; take the same clock
  reading, document it).
- How:
  1. `vcs_store::paths` keeps per green `Head { root }`; extend it with the trees root (declare
     `slates_git::derived::Trees<GreenStep>` beside `GreenPaths`, apply it over the same `GreenStep` change in
     `paths::step`, build whole where the map builds whole) and the head's commit id.
  2. `Commits::make(store, &Fields { tree, identity: Identity::new(name, email)?, seconds, message }, parents)`
     (crates/git/src/derived/commit.rs:312), parent = the green's previous head commit (`Repository::head(store, key,
     index)`, crates/git/src/store/repository.rs:1352; none for a first version); bytes via `Commits::object`; tree
     objects via `Trees::directory`/`names` (tree.rs:350-369); blobs = the map's file contents.
  3. Admit as a single-green store move and record: model on `vcs_store::commit` (vcs_store.rs:973) and
     `repository::push::push_sealed` (push.rs:305; `EntryParts.objects`, `RepositoryLogs::append` push.rs:139). Check
     whether `append` takes an entry with no `BranchChange` (the version already moved via the merge service); if not,
     add an entry kind for "a made commit of version V" (db `RepositoryEntry`; publish any wire/db tag with
     `--test wire_order -- --ignored publish_appended_variants`).
  4. Replay: A-662's `replay_record` re-admits recorded objects; check the made entry replays and that
     `vcs_store::rebuild` equals the live store (the destroyed-branch test's comparison is the model).
- FAILING TEST FIRST (also main's mode check BY USE): git repository, the principal's identities set, a work: write
  `d/a`, chmod +x, `mv d/a x/a`, submit with a message; then read the store: the green's head commit
  (`Repository::head`), its tree (`Repository::tree`), list entries like `git ls-tree -r`: expect the commit's author,
  message and parent, and `x/a` 100755. Red before the change: no commit for the version. If `x/a` reads 100644 that
  is the real bug (the green keys a mode by path, A-618; a rename leaves the mode behind; the first piece's run at
  bad1af546 already saw the map read class 0 there): fix it as its own piece next, failing test first, docs/bugs record.
  Also: a second submit's commit names the first as parent; a push's commit then a made one; rebuild equal.
- Fixture helpers: `crates/server/src/vcs_store/paths/tests.rs` (the fixed history drives works and submits on a git
  repository's root green) and `vcs_store/tests.rs` (`repository`, `work_of`, `appended`, `push_through`, now
  pub(crate) for A-665's hooks). 3-line report after each
  landing and go on; END when context runs short (progress file first).

## LANDED
- Part 3's first piece, main 2bfec1547..0462bd922 (2026-11-19 ~23:10Z): 79e619ea7 (one commit, squashed from
  agents/g308-a664p3 b173aba71 via agents/g308-a664p3-land 9467ce992) + merges 59681646f (main 2f73eb075) and 0462bd922
  (main 2bfec1547). Each git green's map of paths to git's files kept at its head (vcs_store::paths). Runs, macOS arm64,
  Mac Studio: red r124; green r147 (main 3612d1ef9), r242 (b94ae6e90), r289 (9f349d66b: server --lib filtered 175
  passed 2 ignored; merge 18/18; edit_and_submit 2/2, daemon 7/7, works_off_owner 5/5, fleet 14/14; fmt clean), r336 on
  79e619ea7 itself (vcs_store:: repository:: git_push:: git_hooks:: 65/65; fmt clean). run-requests.md entry filed
  (clippy, xtask, doc gate, Linux/Windows, red-checks R1-R5). amendments.tsv A-664 row updated
  (backup amendments.tsv.bak-g308-a664p3).
- NEXT PIECE (main starts it): part 3's commit -- the trees and the commit derived from these maps (A-664 part 2's
  Trees/Commits), admitted to the git store with the green's head moved and recorded with the version in one register
  entry (as a push's objects), replayed by A-662's recovery; then the holders recompute and verify (A-636's fleet half);
  then Mercurial's and Subversion's objects. Also owed: sliced whole builds and releases, a branch's first map from its
  source's, a copy through a work once a client verb makes one.
- Part 2, main 018d32ba2..78a670256: slates-git `derived` (`Trees`, `Commits`, `FileClass`, `Identity`); jupiter
  slates-git --lib 461 passed, 3 ignored; red 3 ways.
- Part 1's record, main 2b3a794b1..b79ece5d5: slates-db `IdentityRecord`, `Op::IdentitiesSet` tag 98; jupiter
  slates-db --lib 144 of 144, --test wire_order 2 passed.
- Part 1's server half, main 54075b520..b13a6e05f (2026-11-19 ~20:18Z, one commit on main, squashed from
  agents/g308-server-half 38742ca1b; the change identical line by line to the one tested). SetIdentities on the
  control channel; the Mercurial user held to what Mercurial 7.2 writes as it is; identities read once a session at its
  first submit; MessageRequired / IdentityRequired at the green's owner before any work is built; contract 51.
  - The first build (5b4a4ce26) stopped at E0599 (`slates_vfs::clock::Clock` not imported in identities.rs); fixed.
  - Wire tags moved at the last merge: A-625 part 3 landed first with RequestBody 87/88 and ReplyBody 74/75, so
    SetIdentities is 89 and IdentitiesSet 76 (Refusal 76 MessageRequired, 77 IdentityRequired unchanged); no
    published tag moved; wire_order 2 of 2.
  - Runs, macOS arm64, Mac Studio: red at 6daffdb98 (the strip checks no-ops: the Mercurial test failed, answers
    [Some(BadRequest), Some(BadRequest), None, None]); green at 2bd393419 over main 545181b30: `slates-server --lib --
    identities verbs::tests deadlines::tests reap::tests repository::tests` 89 passed, 2 ignored, 1 failed (main's,
    below); ipc protocol:: 5 + 1 ignored; wire_order 2/2; api 12/12; client lib 7/7; edit_and_submit 1/1; daemon 7/7;
    verdict_pages 4/4; works_off_owner 6/6; fleet 36/36 (276 s); load_bench built; fmt clean x4. The integration set
    passed alike over b1926d201 and 1b480bd70. The landed tree adds main's 545181b30..54075b520 (slates-git A-653 (2),
    A-654 part 1; slates-merge A-676), which touch none of this change's crates nor `slates_git::derived`.

## RELAUNCHED AGAIN (2026-11-19 ~22:15Z, fresh g308 after the ~22:1xZ interrupt): landing piece 3.1
- Found: agents/g308-a664p3 pushed at b173aba71 (docs ac1c204e5 + e5ab73f3b with red r124 / green r147; merge of main
  225d7eead). The predecessor's last Studio run (~/.cache/g308-logs-p3d, over b173aba71, ended 22:12Z, nothing left
  running): server lib 135 passed 2 ignored; merge lib 18/18; daemon 7/7; edit_and_submit 2/2; works_off_owner 5/5;
  fleet 14/14 (171.5 s); fmt clean.
- Main moved to b94ae6e90 (A-654 part 3 receive-pack over HTTP through push_sealed; A-678 hg import touching
  vcs_store.rs/repository.rs/verbs.rs; A-653 (4)), so a run over the landing tree is owed.
- Land branch agents/g308-a664p3-land, worktree ~/slates-work/g308-p3-land: 6061e8405 = `git merge --squash
  agents/g308-a664p3` over origin/main b94ae6e90 (tree a63b5529e); only conflict the design log's end (main's three
  entries first, this piece's last); every file's +/- lines identical to 225d7eead..b173aba71. Pushed.
- kiln r242 (Studio, script ~/slates-work/g308-scripts/studio-p3e.sh, logs ~/.cache/g308-logs-p3e): server lib
  `vcs_store:: repository:: verbs::tests vcs_http:: identities git_push::`, merge lib, fmt, edit_and_submit, daemon 7,
  works_off_owner 5, fleet 14.
- NEXT: on green, fresh worktree from origin/main, `git merge --squash agents/g308-a664p3-land`, add the run to the
  entry's Runs bullet, one commit with the final message, diff check, push HEAD:main; amendments.tsv row; run-requests;
  report to main; delete Studio clone, logs and target; END.

## RELAUNCHED (2026-11-19 ~21:20Z, fresh g308): A-664 part 3, piece 3.1 continued
- Branch agents/g308-a664p3, worktree ~/slates-work/g308-part3: predecessor's bad1af546 (pushed), merge of main
  3612d1ef9 (d752f0d1b; vcs_store.rs module list conflict only), then dfe4617a8 (pushed): Green::links_naming and
  Green::copied_after (+ engine tests node_tests/copy_tests); paths.rs names each changed path's hard links (a
  regular file's links are never in the change index, so an edit of a linked file left the link stale) and builds
  whole only where no map of the version before is kept or a copy landed in the span (was: any directory at the
  head); paths::forget from repository::retire_member (a destroyed member's map leaked, and a store made again would
  hold none); fixed history 11 versions (v6 edits the hard link's primary; v9's move states the mode explicitly);
  new test t_a664_p3_a_destroyed_branch_takes_its_map_with_it; docs in paths.rs; vcs_store.rs `paths` field doc.
- Predecessor's Studio logs (~/.cache/g308-logs-p3b on the Studio, bad1af546): red 4 of 5 new tests failing with
  advance stubbed; green 105 passed 2 failed (main's repository::tests::each_verb_moves...; and the fixed history at
  version 8: x/a class 0 vs 1 -- the green drops a renamed file's mode, A-618 keys modes by path: FINDING for main).
- NEXT: Studio run (red: paths.rs + repository.rs from d752f0d1b; green focused), docs (A-664 part 3.1 entry, §4.17.1
  status, GAPS), squash-land on a fresh branch from origin/main, report.

## IN PROGRESS (2026-11-19 ~21:00Z): A-664 part 3, piece 3.1 -- the green's state as the path map (A-647's owed map)
- Branch agents/g308-a664p3 (8d1e8a66f, WIP, never compiled), worktree ~/slates-work/g308-part3, on main 733e6de6b.
- Decision (from §4.17.1 item 3's text and A-662's rule that a repository's objects are recorded, derived values not):
  part 3 = 3.1 path map (this) -> 3.2 the commit made from the same changes (Trees + Commits, slates-git derived),
  admitted to the git store with the green's head moved and recorded with the version in one register entry (as a
  push's objects), replayed by A-662's recovery -> 3.3 holders recompute and verify. Mercurial/Subversion after.
- Built: crates/server/src/vcs_store/paths.rs (GreenAt, GreenStep: Graph + PathChanges, GreenPaths = PathMap<GreenStep>;
  advance(state, green, version): derived from the parent's kept map over Green::changed_between, or built whole from
  the head where no parent's map is kept or the change names a directory; lend/settle_in budget; counters
  vcs_paths.derived/built/refused); GitStore.paths declared in made() with the content store's window; hooks after
  verbs::record_accepted (judge_submission) and after push_sealed's apply_planned; slates-merge Green::symlinks,
  hardlinks, specials iterators.
- NEXT: write crates/server/src/vcs_store/paths/tests.rs (fixed history: mkdir+create, overwrite, chmod +x, symlink,
  hard link, fifo (absent), xattr-only, rename file, unlink, rename dir (whole), splice, chmod -x; map == model
  (class, object_id) and root == scratch build; counters built 2 derived 8; plain green no map; vcs_store::rebuild then
  built whole; branch first built then derived) + one push test in vcs_store/tests.rs after push_through; docs (A-664
  part 3.1 entry, GAPS); Studio: red with advance stubbed (sed `return;` first line), green focused:
  `cargo test --locked -p slates-server --lib -- vcs_store:: repository::tests identities` + integration filters
  (daemon/fleet repository submits if any touch) + `-p slates-merge --lib` compile; fmt; squash-land as one commit.

## FOUND (main's, reported 2026-11-19)
- `repository::tests::each_verb_moves_the_names_log_and_each_members_version_its_own_log` fails on main itself at
  3329211e0 (macOS arm64, Studio): repository.rs:762 "not refused: GreenCreated" -- a branch at version 1 is made, not
  refused UnknownBase, so the root no longer folds version 1 after the two writes; alike on my merged trees.
- `crates/server/src/vcs_http/git.rs:47` `pub(crate) use upload::{StoreLender, UploadServices};` warns unused in every
  non-test slates-server build (used by vcs_http/tests/git.rs only): fails clippy -D warnings.
- `deadlines::tests::a_status_capture_is_held_the_failover_delay_from_its_capture_not_its_request` failed once at load
  19.6 over main 1b480bd70 ("the capture was let go within half a window"), passed on main at load 6.5 and after A-660.
- Branches not for main, left as they are: agents/g308-server-half (38742ca1b), agents/g308-server-half-red
  (171622a18), agents/g308-identities (023f2463d), local agents/g308-a664p1-server-land (= main b13a6e05f).

## LEFT (waits on main)
- Main's gates for b13a6e05f: clippy (-p slates-server -p slates-ipc -p slates-api -p slates-client --all-targets;
  watch cognitive complexity in identities::with_session, read_then_route, Resumed::route, verbs::route_gated),
  xtask check, the doc gate, cross-target lint, Windows x86_64 and Linux runs (run-requests.md, g308 entry).
- Part 3 over A-662 (g258, on main since 200aac515): the made commit recorded with the version and admitted to the git
  store; the green's state as the path map (A-647's owed map). Main starts it.
- The clients' surfaces (typed client's submits with a message, SDKs, MCP, CLI, setting identities); Subversion's
  author; Mercurial changesets / Subversion revisions: OWED in GAPS; main starts them.
- Old worktree ~/slates-work/g308-identities (023f2463d + merge 3466fae70, uncommitted stale doc draft): superseded;
  kept as found. ~/slates-work/g308-server-commits (part 2's, clean) kept.
