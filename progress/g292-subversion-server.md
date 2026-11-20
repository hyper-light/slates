# g292 — Subversion server (kiln UI goal 91, 51cef9f7dd5985fda2de4235a5f32653)

Supervisor: main. Clone: ~/slates-work/g292-a500-subversion, branch agents/g292-a500-subversion-design (worktree of
~/slates-work/g066-fs-server's .git).

## Landed
- A-500 on main 22168d111 (2026-11-19 ~01:00 UTC): commits a46ee42f6 (the design §4.17.4, its reading notes
  docs/wip/research/vcs-servers-subversion.md, the A-500 amendment, the GAPS entry, and the new crate `slates-svn` with
  the svndiff 0/2 codec), 9a0c98dbd (rustfmt's layout, the test evidence), merges 03a10b04f and 22168d111 (conflicts:
  §4.17's subsection order and the amendment log's end; nothing dropped).
- Tests: `cargo test --locked -p slates-svn --lib` 18 of 18 on the Mac Studio, before and after `cargo fmt`.

## Filed for main (run-requests.md, 2026-11-19T01:00Z)
- One-crate clippy and `cargo xtask check` for slates-svn; the cross-target lint; an optional stock-Subversion svndiff2
  golden vector.

## Raised
- With g261 (inbox): §4.17.1 items 2 and 5 state Subversion's commit as §4.16's verdict; FSFS's rule is stricter (no
  identity accept, directory property rules, subtree-wide last change). §4.17.4 item 3 states it; a pointer from
  §4.17.1 is proposed.

## Landed (2)
- A-531 on main c7302bc6f (2026-11-19 ~00:50 UTC; UI goal 91 part "The Subversion server built"): commits 629fecd66
  (the ra_svn item codec `crates/svn/src/ra_svn.rs` + tests, §4.17.4 status/ra_svn bounds/piece 2, GAPS, A-531),
  0371d2783 (rustfmt; the bounds test made non-vacuous), 8af917fc3 (evidence), merge c7302bc6f (amendment log end).
  Worktree ~/slates-work/g292-a531-ra-svn, branch agents/g292-a531-ra-svn-items.
- Tests: red first against a stub (13/15, then 14/15 failed), then `cargo test --locked -p slates-svn --lib` 33/33
  before and after `cargo fmt`; fmt clean. Run requests filed 2026-11-19T00:50Z (clippy, xtask, cross-target).

## Landed (3): A-542's first half
- main 6e5f95e56 (2026-11-19 ~01:10 UTC): 2f9951ce5 (NODE_EXCLUSIVE flag + red tests), 13c3a4311 (the verdict pass),
  a5318c15c (a dropped signature line restored), 1bd8de2fd (design: §4.16 exception, §4.17.4 status/piece 3, A-542,
  GAPS; A-500 status sentence restored), f0d45a05e (fmt + evidence), merge 6e5f95e56. Worktree
  ~/slates-work/g292-a542-copy, branch agents/g292-a542-node-exclusive-copy.
- Tests: red with the pass removed 6/8 failed; then 8/8 and slates-merge --lib 11/11 before and after fmt.
- OWED (A-542 second half): Copy on g261's A-534 persistent radix tree (`crates/merge/src/ptrie.rs`: `subtree`,
  `graft`; keys byte-ordered, xattrs keyed path NUL name). g261 messages me the commit when it lands. Plan: per
  current-value map, move the source key to the destination and graft the source's subtree under dst + "/"; copied
  keys' history begins at the copy's version with the source recorded; oracle against a copying reference.

## Next pieces (§4.17.4 "Pieces that follow", main starts each)
3. Copy (A-542's second half, after A-534). Then: node-exclusive operations and `Copy` in the engine (with §4.17.1's shared pieces 1 and 4).
4. The Subversion index with its naive-filesystem oracle.
5. The commit rule as a pure function with a transcription of FSFS's merge() as its oracle.
6. Administration verbs (svnadmin create/load/dump/setrevprop/...), the dump round trip as gate.
7-14. The ra_svn server; locks; mergeinfo; authz; hooks + svnlook; svn+ssh; HTTPv2 then DeltaV v1; review.

## Restart 2026-11-18 (g292 restarted after the usage limit): A-553, the Subversion index (piece 4), IN PROGRESS
- Worktree ~/slates-work/g292-a553-svn-index, branch agents/g292-a553-svn-index, from origin/main cf17cae5d.
- Predecessor clones clean: g292-a531-ra-svn (c7302bc6f landed), g292-a542-copy (6e5f95e56 landed); nothing uncommitted.
- Plan: crates/svn/src/index.rs (+ index/tests.rs): node rows with FSFS's node/copy/copy-root ids (tree.c
  get_copy_inheritance + make_path_mutable, dag.c make_entry/clone_child/dag_copy, 1.14.x d434dc14), per-path history
  lists resolved through copies, per-revision rows, mergeinfo counts; typed refusals for inconsistent records; oracle =
  naive full-tree FS applying FSFS's operations literally (+ drive-order independence check).
- Also owed in this landing (main's clippy finding): crates/merge/src/engine/node_exclusive_tests.rs:322
  `how % 3 != 0` -> `!how.is_multiple_of(3)`.
- FSFS sources fetched for reading only: ~/slates-work/.g292-fsfs-src/ (outside the tree).

## Next piece after A-553 lands (main, 2026-11-19 ~02:10 UTC): Copy, A-542's second half; A-534 on main f8ba01f61
g261's notes (relayed by main):
- Use crates/merge/src/dims.rs: `Dim<V>` = ptrie root + arena (get, insert, remove, iter, prefix, fork, lend).
- Files holds blocks counted by node; xattrs keys = escaped path, NUL NUL, name, so a path prefix ending in '/' is a
  key prefix.
- Add `subtree` and `graft` wrappers in dims.rs (ptrie has both).
- Caveat: under the one history rule a grafted key has no history, so it reads as present since version 0: Copy needs a
  per-prefix appearance record (and per-prefix entries in the change index and last_changed), or it walks the subtree,
  which is forbidden.
- Pattern for setting back what changed: `Lineage::branch(from, v)` in crates/merge/src/engine/branch.rs.

## A-553 progress (2026-11-19 ~02:10 UTC)
- Branch commits: 18a3de6ef (index + tests + is_multiple_of), ba38eed05 (test borrows), 1f77f675d (census counters).
- Studio (kiln, 2026-11-19 02:03 UTC, load 29): red with append refusing every record 19/20 index tests failed (the
  revision-0 test passes alone); green slates-svn --lib 53/53 in 0.25 s; slates-merge --lib node_exclusive 8/8.
- Pending: census+mutation run (kiln server unreachable ~02:08 UTC), design/GAPS/A-553 amendment, merge, push.
- ~02:20 UTC: docs written (design §4.17.4 status + item 2 + piece 4; GAPS; A-553 entry); lint fixes e10e713b5; merged
  origin/main (111 commits incl. A-534) as 0cf39ef9f, one conflict (amendment log end, main first then A-553);
  `git diff --stat origin/main HEAD` = my 6 files. Branch pushed. Waiting on kiln (server unreachable) for the final
  Studio run (census + 3 mutations + green + slates-merge node_exclusive) before pushing to main.

## Landed (4): A-553, the Subversion index (§4.17.4 piece 4) -- main 87bcb392e (2026-11-19 ~02:40 UTC)
- Commits: 18a3de6ef (index + tests + merge crate's is_multiple_of), ba38eed05 (test borrows), 1f77f675d (census
  counters), e10e713b5 (lint fixes; carried the design/GAPS edits), 1fc33474d (design cost note), 9bb51cc11
  (evidence), merges 0cf39ef9f, b6e894ac4, 87bcb392e (each: amendment log end, main first then A-553).
- Files: crates/svn/src/index.rs, crates/svn/src/index/tests.rs, crates/svn/src/lib.rs,
  crates/merge/src/engine/node_exclusive_tests.rs, docs/wip/SLATES_DESIGN.md (§4.17.4 status, item 2, piece 4,
  A-553 entry), docs/wip/GAPS.md (§4.17.4 entry).
- Tests (Mac Studio, kiln): red with append refusing every record 19/20; green slates-svn --lib 53/53 (0.24 s),
  slates-merge --lib node_exclusive 8/8 on the merged tree; census 786 copies (607 self), 70 soft, 48 lazy,
  495 reordered revisions, 12 order-sensitive decisions, all equal; 3 mutations each caught (18/20).
- Run request filed 2026-11-19 (clippy slates-svn+slates-merge, xtask check, cross-target lint, optional deep sweep).
- amendments.tsv A-553 row marked landed.
- NEXT (main assigned, after A-553): Copy, A-542's second half, on A-534 (main f8ba01f61); g261's notes above.

## Piece 5 started 2026-11-19 ~02:45 UTC (main): Copy, A-542's second half, UI goal 91, on A-534's trie
- Main's brief: `subtree`/`graft` wrappers in crates/merge/src/dims.rs; a per-prefix appearance record with per-prefix
  entries in the change index and last_changed, never a subtree walk; O(path length) per copy stated in the design; a
  non-vacuity counter proving no walk ran; an oracle against a reference that really copies, failing first.

## Landed (5): A-542's second half, part 1 -- Copy's maps -- main 1a42140de (2026-11-19 ~02:55 UTC)
- Commits: 354c575f4 (dims.rs docs + `escaped`; dims/copy.rs + tests), ad6c16087 (design §4.17.4 status + piece 3,
  A-542 entry's part 1 and part 2 bullets, GAPS), merge 1a42140de (clean).
- Studio (02:49 UTC, load 27): red with the copies returning nothing 4/4 new tests failed (2 older dims tests passed);
  green slates-merge --lib dims:: 6/6, whole --lib 26/26 (6.2 s). Run request filed (clippy, xtask, cross-target).
- amendments.tsv A-542 row updated.
- NEXT (part 2, owed): the engine's `Copy` (OpKind::Copy = 16; appearance record per prefix; change index and
  last_changed by prefix; readers; identity hash, hard links, root copy to settle) -- plan in the A-542 entry.
  Worktree ~/slates-work/g292-a542-copy-graft (branch agents/g292-a542-copy-graft) is at main 1a42140de.
