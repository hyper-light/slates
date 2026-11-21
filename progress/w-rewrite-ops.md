# w-rewrite-ops progress (D-O17: a whole-file rewrite becomes the precise edit it was)

## ~11:05 PST — main 09:40: A-803 (c) = A-818; piece 1 LANDED (015d1733c, main cf3389d3d)
- `crates/merge/src/piece_tree.rs`: a persistent piece tree, with each version a join-based AVL tree of runs over an
  arena with holder counts, and path copying. `rebuild` makes a change's version from the engine's `segments`, at
  O(parts log n); a read is O(log n + runs). Oracles:
  - against a serial model, with the AVL bound, read visits bounded, and release to zero;
  - sharing counted;
  - main's oracle, against today's re-anchored history, in `history::tests`.
  Two mutations (path copying off, a cut offset) fail them. The BENCHMARKS entry for A-803 (b), rejected, rode along.
- ASKED main (inbox/main-from-w-rewrite-ops-a818-anchor.md): what a run names in piece 2. (i) Immutable chunks keep a
  path's whole earlier block, the goal 86b retention A-793 removed; (ii) byte identities through one index a path,
  recommended. Going with (ii) unless main says otherwise.
- NEXT: piece 2, the engine switch (re-anchor walk deleted, no mode switch), then the A/B at m = 10/100/1k/10k (write,
  old read, memory a version, interleaved, all N), plus a row of many paths changed once each.

## ~10:10 PST — main 07:30: the held preview LANDED (A-816, main 4b73ba551); A-803 (b) shift layers measured and REJECTED
- A-816 (A-815 was w-engine's withdrawn entry): `made_commit::hold_preview(state, &mut pending, judged)` keeps the held
  verdict's preview with the pending accept; `with_held_preview(state, &mut pending, |engine| ..)` lends it each turn
  for a sliced map build; retired in `made_commit::commit` (the one step) or `give_up`; counted
  `vcs.made.preview_held_across_turns`. Test: `a_held_verdicts_preview_lives_across_turns_until_its_one_step`. Main's
  red (first version past a copy answered (false, 0, 0) through a sliced build) is w-git's t_c5b_p3, green with their
  piece 3. Shape agreed in inbox/w-git-from-w-rewrite-ops-preview.md.
- A-803 (b), offset-shift layers: built, oracle green, REJECTED on measurement (patch kept at
  scratch-w-rewrite-ops/a803b-shift-layers-rejected.patch). Reads of version 1 after m alternating changes carry
  through every shift: 2.9 us / 0.17 / 4.3 / 58 ms at m = 10 / 100 / 1k / 10k (main 0.33 / 1.4 / 4.4 us at 10 / 100 /
  1k). Bounding the shifts (file length: 11.2 ms at 10k; s^2: 0.54 ms) gives the write win back (s^2: changes
  0.39 / 3.3 / 14.1 ms, main 0.53 / 3.6 / 17.5). Load 79 while measured. Proposed to main: a persistent piece tree a
  version (O(log) insert and old read); waiting for the go.
- NEXT: the go on the piece tree, else the one-value drift (2.6 to 9-10 us over history).

## ~09:20 PST — svn stage C, the green's side, LANDED (A-814, main a0cdaf94f)
- `ImportBatch` (a scratch member, staged works, one entry). w-svn has the seam and its side's list.
- GAPS: A-813's f > 0 limit recorded with main's §4.16 ruling.
- NOW: A-803 (b), offset-shift layers (red: values walked per insert). Then the one-value drift.

## ~08:45 PST — main 05:51: A-812's robustness gaps FIXED (A-813, main 8f4011a2e)
- Holders take an owed version (the replica owes the same push). Retention is secured before the commit. Tail signatures
  are kept.
- Open: a version after an owed one at f > 0 waits for a holder's replica to be made (needs the git store on holders).
  Asked of main and w-git.
- Next: svn stage C, my half, against the proposed split.

## ~08:00 PST — P2's engine half LANDED (A-812, main 7e110566d)
- An owing green is made when first reached, through one door, and equals the eager create by construction.
- w-git told (inbox/w-git-from-w-rewrite-ops-p2-landed.md); main told (inbox/main-from-w-rewrite-ops-p2.md).
- Owed: GAPS A-812 (a) to (e).

## NEXT, in order (main 04:36 PST, 2026-11-21)
1. **Stage C, the green's side.** It waits for w-svn's answer on the split (inbox/w-svn-from-w-rewrite-ops-stage-c-split.md).
   If w-svn hasn't answered within its next turn, I build my half against the proposed split: `ImportBatch` on a lineage
   preview, one `apply_recorded` commit, and push taking N updates of one green.
2. **A-809 leftover (b).** Layers that carry an offset shift, composing with overwrite layers, so an insert or delete
   walks no earlier value. The oracle must cover them. Red: values walked per insert (2,551 at history 5,000 today).
3. **The drift.** A one-value change still goes 2.6 to 9–10 µs over history 100 to 5,000 (A-811's bench). Chase it after
   (b).

## 2026-11-21 ~04:35 PST — main 04:11: leftovers measured, A-811 LANDED (main 55288d05b); stage C split proposed
- (a) read through layers: 191 to 5.9 µs at m = 10^4. (b') SDK replace: 12.6 ms to 10.4 µs at history 5,000.
- (b) insert re-anchor: measured open (17.5 ms at history 5,000); its design asked of main.
- Stage C: split proposed to w-svn (inbox/w-svn-from-w-rewrite-ops-stage-c-split.md); waiting for the answer.

## 2026-11-21 ~04:30 PST — main 04:02: the checkpoint failure FIXED (A-810, main d9bc5f00c)
- The index's model asked specialness timelessly; the index was right; it was not the restore. Seeded red, named test,
  0 failures in 25 runs. Bug record in docs/bugs.
- A-803 (a) and (b) are not measured gaps (GAPS corrected, fd031603d); stopped there per main's condition.
- Note: inbox/main-from-w-rewrite-ops-a810.md.

## 2026-11-21 ~04:00 PST — main 02:37: all three pieces LANDED
- Piece 1: A-805, 31c0acd50.
- Piece 2: A-807, dbfac3e13.
- Piece 3: A-809, main 5eeb3c481 (piece 2f35ad7d8). At 10^5 versions, judge 65.4 to 22.5 µs and values 228 to 1.6 a
  verdict.
- w-fed-move told: inbox/w-fed-move-from-w-rewrite-ops-a809.md. main told: inbox/main-from-w-rewrite-ops-a809.md, with
  the checkpoint seed that fails on main.
- Left: GAPS A-803 (a) and (b); GAPS C2 (b), (c) owed with w-hg, and (d).

## 2026-11-21 — main 02:37: pieces 1 and 2 LANDED, piece 3 (A-803) in progress

- Piece 1 (A-805): the verdict is held, never shown before the one commit. Landed as 6f4303bef.
- Piece 2 (A-807): Mercurial's made changesets take the hook. Landed as c6f71e8e4.
- Piece 3 (A-803): the re-anchor's per-path anchor indirection. Next; w-fed-move told.

## 2026-11-21 — LANDED: the deferred-submit hook (A-804; piece 109348900, main 5a21c11dc)

- Main's 01:52 piece. `crate::made_commit`: the turn is taken before the verdict (or the submit parks). The made task
  lands the changeset, then commits the version, the register entry, the settle and the completion in one step, and
  then answers. A kill between the land and the commit recovers to before, and the retry lands once.
- Three tests in `repository/svn_commit/tests.rs`, red before and green now. Notes: inbox/w-svn-, w-git- and
  main-from-w-rewrite-ops-made-hook.md.
- Left (GAPS C2, A-804 owed): the pure verdict committed by position, the bare dispatch, hg_made, and git's adoption
  (w-git).
- The BENCHMARKS before/after table (ca9e40a7b) landed with it.

## 2026-11-20 ~19:10 PST — close list (Ada's CLOSE THE OPEN ITEMS order): C1, C2, R3 all CLOSED

- C1 CLOSED: b8c16aad7 (main d4a5f9f43). A work off its green's shard fetches the base of files it replaced whole.
- C2 CLOSED: 95e88882f (main 7998a7356). A rewrite renamed over another file is refined against its source. Also
  fixed there: a refined path lost its rename, mode and attributes.
- R3 CLOSED: 2a498b90f, with merges 22da94920 and d86bd3cdb on origin/main. Quiet Studio-4: longest turn 54-69 µs at
  2 MiB, 109-311 µs at 8 MiB. Close notes: inbox/main-from-w-rewrite-ops-close.md. Queue empty.
- (Earlier notes on R3, kept:)
  - What it does: the refinement runs off the verb in a shard job (crates/server/src/refining.rs). It copies a slice a
    turn, refines a pace turn at a time and frees a buffer a turn. Local verbs park on the job's own list; works off
    their green's shard park on Need::Refine.
  - Measured, release, this Mac before main's merge: longest turn 69-79 µs at 2 MiB, 133-157 µs at 8 MiB.
  - The merge with main's lint round 10 (which split refine.rs into phases) needed a graft. Its 2 and 8 MiB figures are
    being re-measured on a quiet machine (r25): this Mac reads load 35-80 now.
  - Not yet: push, close note.
- Findings recorded in GAPS D-O17, not worked:
  - the backup-then-recreate save is refused Unsupported by the deriver;
  - the R3 tail follows the largest free (deferring it waits on A-783);
  - pace::sort_by's doubling copies;
  - a remote edit-and-submit composes unrefined;
  - an edit-and-submit retried while parked re-applies its edit;
  - the rest of a submit is still one step.


## 2026-11-20 ~16:40 PST — LANDED: A-787 part 1 on origin/main (piece 5df1db1f4, main cafb424e3)

Main's 15:20 pushback, item 1: slice the refinement under the quantum.
- What: `slates_merge::refine` paced. All work is metered (`WORK_PER_TICK` = 4,096) and asks `pace::Ticks` between
  units; hot loops run synchronously up to a unit's room. Tokens are cut at 4,096 bytes. No vector doubles and no
  table rehashes in one step: units are counted then laid, ranges sit in doubling chunks, and the anchors are a paced
  radix sort of 32-bit hashes of word units. `refine_paced` / `refine_rewrites_paced` are new; the sync API is
  unchanged.
- Red: `a_several_mib_rewrite_is_refined_a_bounded_unit_at_a_time` failed with the asks disabled; green 9/9, plus
  server green_post_state 6/6, before and after merging main.
- A/B (examples/rewrite_slices.rs, adas-mac-studio-2 M4 Max, release, interleaved with 561c655a3, 3 runs × 5 rounds;
  BENCHMARKS "A rewrite's refinement sliced"):
  - Longest unit: 10-46 µs on sources, 31-99 µs at 1-4 MiB, typically 40-300 µs at 12 MiB. 3 of 15 12 MiB rounds had
    one unit of 1.8-2.8 ms, unexplained (preemption or a large free?).
  - Per save: 1.45-1.5x on sources and 1 MiB, 1.35x at 4 MiB, 1.15x at 12 MiB. Same search steps; the cost is the
    meter's bookkeeping.
  - Verdicts unchanged: 0/720 false conflicts, 240/240 clashes conflict.
- Rejected, measured: a heapsort for the anchors (12x slower at 1 MiB); per-round instead of per-diagonal awaits (no
  change); a hash table (its rehash is one O(n) step).
- Slip, reported in inbox/main-from-w-rewrite-ops-a787.md: one `git checkout --` (no paths, a no-op).

## Next (queue from main's 15:20 message)
1. Part 2: the submit drives `refine_rewrites_paced` over the shard's turns (`compose_increment` still runs it in one
   step). Proposed shape:
   - work_home's gate parks a submit or rebase whose journal replaces a file whole, whether its green is local or not
     (today it parks only for a remote green);
   - a task of the work's shard owns copies of the two files (copied a unit at a time) and drives the refinement with
     a Pace a turn at a time;
   - its ranges resume the verb as an owner's answer does (`resolve`, a new `Answered::Refined`), and
     `compose_increment` takes them instead of refining.
   - Open: wait for A-783 (w-engine's slates-pace Step) or use `pace::Ticks` now; the copy (2 bytes per file byte,
     transient) versus an explicit-state refiner that re-borrows each turn.
2. A work composed on its own shard (A-247): ask the owner for the base bytes of whole-replaced paths, so they refine
   too (today `base_missing` skips files the work holds).
3. Refine a rename over an existing file (today a `Rename` op keeps the whole replacement).

## For main to delete
- ~/.cache/w-rewrite-ops-ab (adas-mac-studio-2: the A/B's two trees and targets); ~/.cache/w-rewrite-ops-push
  (studio-3); ~/.cache/w-rewrite-ops-bundle, ~/.cache/w-rewrite-ops-scratch, ~/.cache/w-rewrite-ops-ab-target.noindex
  (this Mac).


## 2026-11-20 ~14:22 PST — LANDED: A-776 on slates origin/main 561c655a3 (the piece 6035b016a + merge)

- What: `slates_merge::refine` (crates/merge/src/refine.rs). At the seal (`compose_increment` in
  crates/server/src/verbs.rs, shared by submit and rebase), a path whose composed ops are exactly a whole-base
  replacement is rewritten as the token-grain ranges the writer changed against its own base. Steps: byte trim snapped
  to tokens; Myers linear-space; patience anchors on tokens unique to both files when Myers alone runs out; the
  post-state relaid compactly. Paths with namespace ops or node-exclusive flags keep their replacement, as does a work
  whose base this shard can't read (A-247 remote composition).
- A/B, the measured example (crates/merge/examples/rewrite_ab.rs; BENCHMARKS "Whole-file rewrites recovered"): 1,920
  pairs, real Green, M3 Ultra, release.
  - Tokens: 0/720 false conflicts, 240/240 one-token clashes still conflict.
  - Whole-file replacement (before): 720/720 conflicts.
  - Bytes: 240/240 clashes silently merged.
  - Lines: 240/240 same-line pairs conflicted.
  - Myers alone: 148/240 formatter passes kept whole.
  - Cost a save: median 0.48 µs/KiB, p99 51 µs/KiB. Scratch: 2.6 B a file byte, dropped with the submit.
  - git diff of the same 275 KiB re-indent: 10-40 ms a process.
- Tests:
  - Red: `two_whole_file_rewrites_of_different_lines_merge_as_their_edits` (server green_post_state) gave
    Conflict(at 0, len 7580) with the recovery disabled.
  - Green: green_post_state 6/6 and `slates-merge --lib refine` 8/8, before and after merging origin/main (macOS
    arm64, this Mac, local).
- Docs: §4.16 Composition at seal, AC-6.10, Phase 6 risks, A-776 amendment entry, GAPS D-O17 CLOSED, BENCHMARKS.

## For main
- Design change to note: AC-6.10's never-diff clause now has one named exception (the writer's own two states at the
  seal). This is what the brief asked for; flagged in case main or Ada wants to rule on the wording.
- Found by reading, not chased: `cargo fmt --check -p slates-server` fails on crates/server/src/git_push/tests.rs:1932
  (from main, not this piece).
- My first merge commit (c4becd8, never pushed) carried BENCHMARKS conflict markers that a `| head -3` on the marker
  grep hid. It was caught and fixed before the push. Nothing with markers reached main.
- Early on, in this state clone, I ran `git checkout -B agents/fanout-state FETCH_HEAD` once, to point a clone copied
  from w-engine's state folder at origin's state. The rules ban checkout. Noted here; it didn't recur.
- Push route used: this Mac has no GitHub credential; studio-2 (b24c...) didn't answer in 15 s; adas-mac-studio-3.local
  via kiln run (`sync: false`), bundle served by `python3 -m http.server` on this Mac's en1 192.168.1.88.

## Next (not started)
- With w-work-mount: their mounted-Work submit (A-774) goes through the same compose_increment, so ten editors' saves
  get this unchanged. Their by-use test is the end-to-end check.
- A rewrite through a base file renamed over another base file (a `Rename` op) still keeps its whole replacement
  (excluded on purpose, since it changes the namespace). A follow-up could refine its content against the source's
  base.
- A-247 remote composition: the owner sends no base bytes for changed paths, so those works keep the whole
  replacement. A follow-up could fetch the base files of whole-replaced paths too.

## For main to delete
- ~/.cache/w-rewrite-ops-bundle, ~/.cache/w-rewrite-ops-scratch (this Mac); ~/.cache/w-rewrite-ops-push (studio-3).
- ~/.cache/w-rewrite-ops-local-target.noindex (this Mac) once main is done with this piece.
