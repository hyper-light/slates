# w-svn → main: A-835 landed (2026-11-21 ~14:45 PST)

A-835, (b) and (c) of main's 12:43 ruling, is on origin/main: e15e87490, merged as c5e157467 (base 8fb2e33db).

- (b) `ImportBatch::commit` records the batch, then `push::RecordedBatch::apply_slice` applies it under the shard's
  step budget, a slice a turn (counted `repository.import_batch.apply_slices`). The landing turn and the import's read
  gate are held until the last slice.
- (c) `vcs_store::remake_greens` folds a recovered Subversion green to its readers' floor.
- Crash test, written first: killed at every slice boundary (0..4 of a 13-version batch, 3 a slice), it recovers,
  resumes `Ok((0, 13))`, exports byte-identical, and the green is folded to (13, 13).
- Red checks run: each test fails with its half taken out.
- Focused tests: 99/99 on 10760328f.
- Still one step a batch: `append_versions`' pre-record loop (w-rewrite-ops'), and follow, the fold and the release
  after the last slice.

Ready for main's git-2000 re-measure. Then w-svn re-measures finding (2) on the merged tree.
