# An import's landings overlapped, unseen, where every turn cut a segment

- **Date:** found 2026-11-27, on diag CI 6 and diag CI 9's Windows lanes (red 4), and diagnosed the same day:
  - main's runs 46045562251 (windows-11-arm), 46047367289 (windows-latest) and 46051267769 (both lanes), on the
    `w-interop/hg-landing-diag` instrumentation, never main;
  - main's ruling (a') of 10:53.
- **Status:** fixed (the counter).
- **Subsystem:** the Mercurial import's pipelined segment landings (`crates/server/src/vcs_store/hg_import.rs`,
  `SEGMENTS_LANDED_OVERLAPPED`), and the store owner's Land answer (`crates/store/src/owner.rs`, `Landed`).
- **Severity:** a red test on Windows, x86_64 and arm64. The import was pipelined all along; only its counter missed
  the overlap.

## Symptom

`vcs_store::hg_import::tests::an_imports_file_texts_held_between_landings_are_bounded_by_its_segment` failed at
`tests.rs:824`:

```
landings answered while the import went on (`vcs_store.hg_import.segments_landed_overlapped`): Held { most: 1057396,
at_end: 8694, bound: 524288, by_texts: 63, overlapped: 0, waited: 2545800, landing: 3594218800, chunked: (67, 0) }
```

## Root cause

An import step runs in this order: a yield beside the landing in flight, one turn, then the hand-over. The hand-over
awaits the landing in flight and begins the next. The counter counted only an answer seen at the yield.

The segment's bound is the content chunk (16 pages) times the store's fanout (8): 2 MiB on a 16 KiB-page Mac, and
512 KiB at a 4 KiB page. The test's texts are 1 MiB each, so at 4 KiB every turn cuts a segment. The hand-over then
follows the turn in which the landing ends, with no yield between, so its answer is first seen at the hand-over.

Main's read of the extended diag (windows-latest, run 46051267769) shows the landings did overlap:
- in 64 of 67 landings the owner sent its answer before the hand-over began (sent p50 0.81 ms, hand-over p50
  49.3 ms, the turn p50 49.07 ms);
- the other three are the import's last landings, awaited at its end.
- windows-11-arm (same run, job 140682882817) agrees: 63 of 67 sent before the hand-over (sent p50 3.51 ms, hand-over
  p50 78.4 ms, the turn p50 78.12 ms), and the other four are the import's end.

The MacBook at a 4 KiB page has the same shape: 67 landings, `overlapped: 0`
(`~/.local/state/w-interop-logs/red4-page4k.log`).

## Exact edits

- `slates_store::owner::Landed`: a landed changeset and `sent_ns`, the owner's send instant on
  `slates_machine::clock::monotonic_ns`, read once a landing on the owner's thread. `StoreHandle::land_stamped`
  answers it; `land` is unchanged for its other callers.
- `hg_import`: a landing counts as overlapped where its answer was seen at a yield, or where its owner sent it before
  the hand-over began (`Met`). A landing begun and awaited at once is sent after its wait began and is not counted,
  so the counter cannot certify the serial shape.
- The test also runs at a 4 KiB page (`an_imports_landings_overlap_where_every_turn_cuts_a_segment`) and requires the
  bound to be under one text there. The run at the host's page alone was CLAUDE.md's degenerate-parameter gotcha.

## Verification

- The red: the Windows lanes above, and the MacBook at a 4 KiB page (a scratch run, before the fix).
- After the fix, on the MacBook (debug, 2026-11-27): both held-texts tests passed, 2 of 2 in 78 s, and
  `cargo test -p slates-store --lib -- owner` passed 6 of 6.
- Owed (main): the next diag CI's Windows lanes.
