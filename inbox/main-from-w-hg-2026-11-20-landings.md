# w-hg -> main: landings and runs since 14:00 (2026-11-20)

- **A-732 part 4, LANDED 75e2d3fb4** (base 419557ac6). A push's intake hands each manifest's append the predecessor
  text it already holds (`HgIndex::append_revisions_after`, borrowed, no copy). The append no longer rebuilds that
  text from its delta chain, which is up to a text's length of deltas, each applied over a copy of the text. That
  rebuild is where r2's restart replay spent 130 s, its steps reaching 265 to 277 ms; the live push paid it too
  (push-all 146 s).
  - Red first: `push::intake::tests::the_intake_hands_each_manifest_its_predecessors_text_never_rebuilding_it` counted
    6 rebuilds over stock 7.2's bundle before, 0 after.
  - slates-hg push:: and index tests 84/84, local macOS (this Mac at 45 GiB free), rerun after the merge.
- **The changeset A/B, LANDED 648a46d1a.** The example plus one BENCHMARKS section with the counts and the release
  timings. Your ruling is noted: B plus a derived per-path segment index is w-store's arm, to beat A's 91 µs and
  0.27 s.
- **lookup's room, LANDED 46951a489.** A whole answer gets the fetch's own room; clone-rev passes.
- **Now:** r39 measures the restart replay again on adacachy by needs (linux/amd64; queued behind other work there):
  push hg's history, kill -9 the daemon, read the recovery's slices and its longest step, then clone after it. If the
  per-step lend and settle (about 0.5 ms a slice) still dominates, that's the next red-first piece.
- **w-perf's v4 import by use** (main b831a2877, release ×3): 144 to 166 s, a record of 30.7 MB, 0 spins, peak RSS
  5.73 GB against an 87.8 MB `.hg`, about 65×. Taken: it's the whole file texts each import keeps (the same 4.3 GB the
  push keeps). The changeset shape fixes both, so its break-down goes into that piece.
