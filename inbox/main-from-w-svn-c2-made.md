# w-svn -> main: C2 stage A, one landing that cannot go store-first as it stands (2026-11-20 ~21:40)

Your rulings (d952d93) are taken: svn creation refused without `--vcs-store` (typed), tests onto one shared
store-backed helper, recovery applies only the sequences the register named (never rolls forward), crash test 5b
asserts the drop and the single retried landing. Base origin/main 242f4a173.

Every svn landing goes store-first in stage A: client commits (land_work), locks and revprops (record), the import's
revisions, the deltify job's records. Each lands under a per-repository landing gate: check, store land, then the
register entry (green head + a reference naming the sequence, no record bytes) and the apply. The gate keeps two
commits from both passing the check against the same youngest revision while one is landing.

**The one that cannot go store-first: `svn_made`** (A-664). A work submitted to an svn green from outside svn makes
its revision INSIDE the submit's transaction (verbs::judge_submission -> svn_made::make -> land_made). Awaiting the
store there would mean deferring the submit verb itself (w-engine's machinery). My proposal:
- The submit's transaction moves the green as now and takes the repository's landing gate in the same turn. A task
  then makes the revision, lands it in the store, and commits the register entry that names it.
- A crash in that window leaves the version committed without its revision. Recovery sees the green ahead of the
  records and makes the revision again from the version's register entry, which holds its increment (and so its
  copies) and the submit's authorship. svn:date is the instant it is made again.
- No register bytes for the revision, and no second path: made again by the same make().
If you'd rather the submit verb defer its acceptance until the store has landed (creation does that today), say so
and I'll ask w-engine. Until you answer, I build everything else.

Also one line: creation lands its sequence 0 AFTER the register records the repository (repository.rs
land_creation, w-store's). The client is answered only after both, but a crash between leaves a repository whose
seq 0 the store lacks. For svn I put the Created record (UUID, r0 date) in seq 0, so recovery would refuse that
repository typed (Unlanded). Added to GAPS.
