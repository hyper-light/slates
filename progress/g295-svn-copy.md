# g295 — the merge engine's Copy (A-586; kiln UI goal 92, 51cef9f7; A-542 part 2)

Supervisor: main. Worktree ~/slates-work/g295-svn-copy (a worktree of g066-fs-server's .git), local branch
agents/g295-a585-svn-copy (named before the A-number was fixed; pushed as agents/g295-a586-copy).
Policy read 2026-11-18: restart preamble (THE RULE), slates CLAUDE.md on origin/main (§7, §8), design §4.17.4, A-542.
The predecessor left no commits and no uncommitted work.

## Landed: A-586 on main ce7f17865 (2026-11-19 ~04:55 UTC)
- Commits: 342218c19 (Copy in the engine: ops_doc, engine.rs, engine/copy.rs new, branch.rs, dims, ptrie, chain_entry,
  map.rs, tests), fa6acbe94 (rustfmt; test fix), 91a40198a (cost test ordering; census print), 4d55bfeba (oracle
  widened), 4dbfd1562 (docs: §4.16, §4.17.4, A-542 pointer, A-586 entry, GAPS), merge 02c16c92c, e12c76ac4 + 5998d368a
  (reanchor on a copied file, test first then fix), 54be484ef (evidence), merge 04a3a7f98, a0d2b995b + f5f1fdc05
  (copied_in guard against A-612's rename-onto rule, test first then fix), 4511eb2b6 (evidence), merges b4a0b982a and
  ce7f17865.
- Files: crates/merge/src/{ops_doc.rs, engine.rs, engine/copy.rs (new), engine/copy_tests.rs (new), engine/branch.rs,
  engine/reanchor.rs, chain_entry.rs, dims.rs, dims/copy.rs, dims/copy/tests.rs, ptrie.rs, map.rs},
  crates/merge/tests/ops_doc.rs, docs/wip/SLATES_DESIGN.md, docs/wip/GAPS.md.
- What it is: OpKind::Copy = 16 (path dst, src = source path index, at = source version), node-exclusive as an
  addition; one appearance record + one change-index + one last_changed entry for the destination, never per key; a key
  reads by the latest copy above it while it holds that copy's value; a key's first change after the copy seeds the
  copied value at the copy's version (none in the copy's own version); same-increment ops on the copied tree judged
  against the copied tree; chain entry Applied::CopyTree (tag 18, version read back from the increment); Lineage::branch
  before a copy cuts it (Ptrie::cut); identity = state entries + per copy (its entry - the copied tree's entries), O(1)
  a copy; hard links in the source refused by kind (TypeChanged); the root a source (svn copy ^/ ^/snap: client refuses
  own-child copies only WC-to-WC, libsvn_client/copy.c:3089) and never a destination.
- Interactions fixed on merging: A-596's Green::reanchor (a comment on a copied file now moves); A-612's file_removed
  (a file the version's own copy brought and its increment edited records no removal: Green::copied_in).

## Tests (Mac Studio, kiln, 2026-11-19 04:26-04:52 UTC, load 5-11, debug, fresh clones)
- `cargo test --locked -p slates-merge --lib`: 44/44 at 91a40198a and 4d55bfeba; 52/52 (2 main's ignored) at
  5998d368a; 58/58 at f5f1fdc05 (merged with main twice; crates/merge identical at the landing). fmt --check clean.
- Oracle census (128 histories, f5f1fdc05 run): 58 accepted, 594 refused, 5 from root, 23 from an older version, 9 into
  their own tree, 107 refused for a changed source, 11 keys changed beneath a copy.
- Red: six mutations each caught (readers 7 tests, identity 4, occupied 2, branch cut 1, source changed 2, no split 2);
  reanchor fix and copied_in guard each red first (their one test alone failed).
- Studio scratch ~/.cache/g295-a586 deleted (1.6 GB).

## Filed
- run-requests.md 2026-11-19 (clippy + xtask, the crate's integration suites incl. ops_doc.rs's updated round-trip,
  workspace check for dependents, cross-target + doc build).
- amendments.tsv A-586 marked landed.

## Owed (A-586's, in GAPS' §4.17.4 entry; main starts each)
- checkpoint of a green holding a copy (tree listed + identity sum carried; `None` today, chain kept whole);
- rebase restating a copy (deriver's VolumeOp::Copy; TypeChanged today);
- copy from a version its source changed after (set back at the destination + recorded in the chain entry);
- Subversion's replace with history (copy onto a path its own increment removes);
- moves / links / second copies into a copied tree in one increment;
- the server's change-index consumers treating a copy's destination as its tree;
- design decision for main/Ada: a Merkle identity over the radix trees (A-202 format change), weighed in A-586.

# A-626 part 1 (UI goal 93): Copy's replace with history and copies nested in one increment -- LANDED main 9e514ef36

## What landed (2026-11-19)
- `REPLACE` (0x01) on `Copy`'s flags (crates/merge/src/ops_doc.rs, `Op::replaces`): the destination's tree is taken
  away key by key at the copy's version (files as unlinks, IPC nodes, dirs, symlinks, then every mode/attribute via
  A-618's drop_metadata), then the source grafted; judged as a removal (DeleteModify if changed after base, TypeChanged
  if a hard link or link target) plus A-586's addition. Chain entry unchanged in size: the flag is read back from the
  increment's op through A-632's Declared index (now (version, replace)).
- Nested copies in one increment: judged against what the outer copy brings there (present needs REPLACE,
  CreateCreate otherwise); svn move inside a copied tree = copy + unlink, lands. Renames/hard links into copied trees
  stay refused (TypeChanged). Every source held before any copy lands (FSFS copies from a revision root); copies land
  in destination order.
- Readers: own removal at a copy's version counts as before the copy (covered_since tie); nearest copy wins a
  same-version tie (copy_above). Reservations: history + changes reservations add the replaced tree (O(tree) walk).
- Commit refactored: apply_effect over a Staged set (removed, set as StagedFile, emptied for A-618), remove_file_now,
  set_symlink/set_hardlink/set_xattr; main's A-618/A-631 changes folded in at the merge.
- Three latent A-586 branch defects found by the new oracle and fixed (branch.rs): cut-tree keys equal at both ends
  were left cut; non-file dims read at the branch version from raw history (ignored older copies); copy entries given
  back only for keys changed after the version (missed a destination whose history ended before the copy).
- Main's clippy findings fixed: head_identity_from_scratch split (add_state_entries); branch.rs lasts array gone
  (HasHistory slice).
- Oracle rewritten as a per-version snapshot model (no prop_oneof!, weighted selector; no expect in Result fns):
  puts/takes/mkdirs/copies (replace or not, older versions)/nested pairs/moves; bases, bytes, change index named sets
  at every version; identity == recount; replay; branch at every version (walk-free) vs model + replay identity.

## Tests (Mac Studio, kiln, 2026-11-19, debug, fresh clones of agents/g295-a626-copy-cases)
- r851 (fcffd15c4, pre-merge): 61/63, both failures test bugs; red over 6ce9e687d's engine: 7 failed, 10 passed.
- r868 (682479e9f, merged, 05:51 UTC): 95/97 (the branch defects); red over main 0020d61's engine: 7 failed, 10 passed.
- r893 (eddac8c74, 05:59-06:03 UTC, load 5-6): 99/99, 24.48 s, fmt clean; census 85 acc / 362 ref / 8 root / 33 older /
  25 into self / 71 changed / 33 beneath / 31 replaced / 31 nested (7 inner replace) / 20 moves; red seven ways:
  tie 2, nested 2, changed-after-base 1, metadata 4, reservation 1, lazy sources 1, branch.rs pre-fix 4.
- 2d453fca3 (merged again, 06:08 UTC, load 10): 111/111, 24.48 s, fmt clean; census 80/397/4/30/14/63/34/31/34/4/29.
- 9e514ef36 landed: main's newest touched no slates-merge code (code = 2d453fca3).
- Studio scratch ~/.cache/g295-a626 deleted after.

## Filed
- run-requests.md 2026-11-19 g295 (clippy, xtask, integration suites, doc build, workspace check).
- amendments.tsv A-626: part 1 landed.

## Owed (A-626's later parts; design notes)
- Part 2, copy from an older version of a changed source (FSFS accepts): at commit, for each source key changed in
  (r, H], set the destination key to its value at r (corrections; cuts of nested copies made after r; start empty if an
  ancestor copy after r); record the corrections in Applied::CopyTree so replay reads no folded history; a green-side
  reservation for them; the server's entry_bound (crates/server/src/verbs.rs, from the increment alone) needs room --
  file that wiring for main. Oracle: drop the model's changed_after refusal for sources.
- Part 3, checkpoint of a green holding a copy: format v4 carrying copies (appearance records), corrections and the
  identity's lanes; restore in version order. Today `checkpoint` answers None at/after a copy's version.
- Part 4, rebase restating a copy: the deriver's VolumeOp::Copy (TypeChanged today).
- Weighed, owed only if measured worth it: replace by reference (held old tree in the record) instead of key by key.

# A-626 part 2 (UI goal 93): a copy from a version its source changed after -- LANDED main 6fba33d56

## What landed (2026-11-19)
- Copy of S@r with S changed in (r, H]: graft the head's S and fix it (`chain_entry::CopyFix`): keys at paths the
  change index names in S since r set to their values at r (only where they differ from the graft); copies made since
  inside S cut (outermost); a copy made since over S (replaced_after) grafts nothing (start_empty), every key then is
  named by its removal. O(change-index entries since r + keys at those paths); no map walk (attribute names come from
  their histories; a name with no history reads alike at both ends).
- Commit: graft (unless start_empty), cut, set values; no history, no identity marks (the copy's entry stands for S@r;
  copied_tree_entries reads the source at its version).
- Verdict view `SourceAt` (engine/copy.rs): copied-tree ops, nested places and "anything stood there" read S@r; the last
  counted from radix-tree node counts (`Ptrie::prefix_len`, `Dim/Files/Xattrs::count_tree`, `Green::count_tree`).
- Chain entry: record kind COPY_TREE_FIXED (19) = COPY_TREE + the fix; counts checked before allocation, duplicates
  DuplicatePath, bad flag BadKind; tag 18 unchanged so old entries read as before. Replay: `EntryCopies` gives an edit
  beneath a fixed copy the bytes the copy brings.
- Room: `Green::copy_entry_room(inc)` = Σ `fix_bytes` (== the encoding's length, tested); crates/server/src/verbs.rs
  adds it to `entry_bound` before `Op::GreenAdvanced`'s admission (the green is looked up first; NotFound as before).
- Refusals: version below the fold floor / restore checkpoint with a changed source -> TypeChanged; hard link at r ->
  TypeChanged. Unchanged sources copy from any version (A-586).

## Tests (Mac Studio, kiln, 2026-11-19, debug, fresh clones of agents/g295-a626-part2)
- 69936d04d (06:34 UTC): 117/118 -- oracle: the copy walked (head attribute names listed); fixed in 4ff0b7131.
- r1027 4ff0b7131 (06:38 UTC, load 9.4): slates-merge --lib 118/118 (24.40 s), fmt clean (merge + server); census
  75/354/9/40/24/14 from changed/25/28/31(2)/19; slates-server --lib 590 passed, 3 ignored, 1 failed (main's, below).
- r1040 (06:42-06:48 UTC): red seven ways -- feature off 4, no cuts 2, no start_empty 2, fix not recorded 4, replay
  head bytes 1, verdict head bytes 1, room 0 1.
- e728908e6 (merged, 06:50 UTC, load 8.3): slates-merge --lib 133/133 (25.95 s), fmt clean; census
  106/390/23/60/40/23/37/48/46(2)/26; slates-server --lib 614 passed, 3 ignored, 1 failed (main's).
- 6fba33d56 landed (main's newest touched no slates-merge code nor verbs.rs).
- Main's failing test (sibling, reported): `deadlines::tests::a_status_capture_is_held_the_failover_delay_from_its_
  capture_not_its_request` (crates/server/src/deadlines.rs:830, "the capture was let go within half a window of its
  capture") fails 5/5 alone and 3/3 with main's own verbs.rs at d449a10c7 and dc46c9f.
- Studio scratch ~/.cache/g295-a626p2 deleted after.

## Owed (A-626 parts 3 and 4; ended here on context, as main allowed)
- Part 3, checkpoint of a green holding a copy: today `checkpoint(at)` answers None at/after a copy (`copied_by`).
  Needs a checkpoint format version carrying the copies' records, the copied trees' keys (a restore sets named keys;
  copied keys have no history so are not named today) and the identity's running sum (a copy's entry stands for its
  tree, so a recount of the restored state differs); golden vectors; the server's `merge.checkpoint.unavailable`.
- Part 4, rebase restating a copy: the deriver's `VolumeOp::Copy` (Green::rebase answers TypeChanged on a copy today).
