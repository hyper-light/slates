# w-hg -> main: Mercurial's replay at restart, sliced and allocating less (2026-11-20, after 15:00)

Landed on main, each red first, focused tests local (this Mac at 130 GiB free):
- **A-732 part 4, 75e2d3fb4.** The intake hands each manifest append its predecessor text, borrowed. r2's 130 s and
  265 to 277 ms steps rebuilt that text from its chain. Red: 6 rebuilds over stock 7.2's bundle before, 0 after.
- **revision_text, dc59ef2a1.** A chain is applied into two swapped text buffers and one reused delta buffer, so a
  chain costs its two longest texts and its longest delta, not a text a link (w-perf's `apply_into`). slates-hg lib
  371/371.
- **A-732 part 5, 6935d8e86.** A recovery unit, or a rebuild's turn, takes the record's steps under one lend and
  settle until its slice's budget is spent. Before, every revision was its own unit: 259,321 units for hg's history.
  Counted as `vcs_store.hg_replay_turns`. Red: `a_recovery_unit_replays_as_many_revisions_as_its_slice_allows`, 31
  units for 15 revisions before. slates-server hg_import/hg_push/hg_view 32/32 after the merge.

**Owed: the measurement.** A release restart of hg's history: push, kill -9, read the recovery's slices, longest step
and CPU, then a clone. r39 on adacachy (by needs) couldn't fetch hg's repository (mercurial-scm.org "incomplete
response", 3 tries), and its retries were refused "no room here now". The Studios aren't reachable for sync. I'll run
it as soon as a Linux or Studio runner takes it.

**Next, unless you order otherwise:** item 6, a forced new-head push on a moved line, judged as a work and made over
the head (stock 7.2's `push -f`). Then the changeset shape with w-store, once its arm settles the layout.
