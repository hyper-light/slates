# main from w-rewrite-ops: piece 1 IS on main, as its own commit, before A-807 (2026-11-21 ~03:40 PST)

Piece 1 (A-805) is commit **31c0acd50** on origin/main: "server, merge: a server-made commit's verdict held with its
landing, shown to no reader before its one commit (A-805)". It landed before A-807 (dbfac3e13). My earlier note cited
the push clone's merge hashes (6f4303bef), which is why it was hard to find.

What it does:
- With the landing turn held, the verdict is judged and held (`Green::judge` / `judge_computed`, `verbs::verdict_of`'s
  hold). Nothing is applied in the submit's step.
- The revision is read from a preview: `Lineage::preview` makes a branch at the head, O(1), and commits the held verdict
  into it as its version 1. The preview is retired in the same step.
- The verdict is applied to the green only in the one commit (`verbs::apply_accepted` → `Green::apply_recorded`, the
  replay recovery runs), together with the version, the register entry and the completion.

Tests (red on A-804, green since):
- `crates/server/src/repository/svn_commit/tests.rs`:
  - `a_read_of_the_green_while_its_made_commit_lands_sees_the_old_head`: a read in the submit's step saw
    `(1, Some("alpha\n"))`, the head and the new file. It now sees `(0, None)`, then `(1, the bytes)` once committed.
  - `a_refused_made_landing_leaves_nothing_any_reader_saw_and_its_retry_lands_once`: the store refuses every changeset.
    No reader saw anything in the step or after the refusal; nothing is durable; the retry lands once.
- `crates/merge/src/engine/branch/tests.rs`:
  `a_held_verdict_previewed_reads_as_its_commit_while_its_green_shows_nothing_of_it`. The preview reads as the commit
  does (file, identity, changed paths), the green shows nothing, and a moved green is refused `Moved`.

So the order you asked for held: correctness (A-805, A-807) first. A-803's fix (A-809) is built and is in its A/B now.
