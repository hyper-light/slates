# g081 — progress (REWRITTEN 2026-11-18 ~00:10Z from my own context after the ~/.cache wipe)

POLICY READBACK (2026-11-18, role: implementer subagent g081 of the slates supervisor "main"; slates CLAUDE.md §8 on
origin/main + restart preamble rewritten ~22:10Z). ONE piece: change + unit tests (a bug's failing test first,
red-checked once) + design/GAPS lines. Run its `--lib` unit tests, `cargo fmt`,
`cargo clippy --locked -p <each crate touched> --all-targets -- -D warnings`, `cargo xtask check`. Merge myself:
fetch, merge origin/main, read `git diff --stat origin/main HEAD`, `git push origin HEAD:main` as its own command.
Never checkout/switch/stash/reset/force/commit-tree; branch with `git worktree add -b`. Builds on the Mac Studio
d1323898 (sync false, GitHub clone of my pushed branch, own CARGO_TARGET_DIR under ~/.cache there, deleted after),
never on this MacBook. END: heavier runs to run-requests.md, a few-line report to main, stop. Zero arbitrary
constants: every tunable learned at runtime from the deployment's own data.

## A-494 (UI goal 90, kiln c2e4337b) — the archive import decodes its file in bounded slices: LANDED, ENDED
- On main d0e39b52b (merge of agents/g081-a494-sliced-import: 60cb771d7 the change, b5fa5408f a borrow fix, 94f5c5759
  fmt + test fixes, 91954a02a the oracle's count + its seed file, fadc81b76 evidence), pushed 2026-11-19 ~00:50Z.
- What: `slates_archive::Decoding` fed the stream in order (`new(len)`, `feed`, `step`, `bound`, `LONGEST_FIELD` =
  116 bytes; `Archive::decode` feeds it whole: one code path; `ArchiveError::BadLayout`, classed `Malformed`); the
  checksum is checked before any payload or manifest is decoded, a framing fault kept until then. The server's
  `ImportJob` feeds it a derived feed slice a unit (`feed_slice_bytes` = c*h/(2h+c) of the copy and BLAKE3 slices),
  holds at most `Decoding::bound` of the file (counter `image.archive_import_stage_past_bound`). Design §4.11 status,
  A-494 entry, A-453/A-459 owed lines; GAPS A-453 entry.
- Unit tests (Mac Studio, debug, 2026-11-19): `cargo test -p slates-archive --lib` 9/9 in 0.62 s; `cargo test -p
  slates-server --lib image::tests` 2/2 in 0.76 s; fmt clean. A proptest seed found the oracle's own count wrong for a
  stream refused at its header; fixed, seed kept in crates/archive/src/archive.proptest-regressions.
- Not run by me (main's orders 2026-11-19): the red-checks M1-M3, clippy, xtask, suites, cross checks: filed in
  run-requests.md (2026-11-19T00:55Z g081).
- Owed (A-453's, in GAPS): the decoded archive is held beside the restored tree until the restore ends; the manifest's
  decode is one piece.

## Held: UI goal 83 (was 81; kiln 21d6f67c) — the Codex agent's Green/Work and atomic-conflict work lands on main
Waits until goals 84-90 are done (main, 2026-11-18). Triage so far (by the code, 2026-11-18):
- Clone ~/slates-work/g081-codex-lands holds the 575 Codex refs under refs/codex/* (equal to adacachy's refs except
  writable-dax = the base c8d6adb4a). 1,537 commits beyond c8d6adb4a, none on main; main is 2,889 commits past it.
- ON MAIN (superseded): the baseline commit in all 575 refs is adacachy's uncommitted A-135 tree; A-171 replaced A-135.
- SUPERSEDED ON MAIN: the mapping/keeper/strict-map/pin/read-map/stream-frame/DAX-ACK families (built on A-135's pinned
  arena map; A-171 and A-195 replaced and removed it).
- CANDIDATE 1: the merge splice fix (Codex 463636027; crates/merge/src/splice.rs byte-identical on main, so its rescan,
  unchecked length sum and unchecked op order are on main; no product caller of merge::splice on main yet).
- CANDIDATE 2: per-range identity for length-changing overlaps (Codex bedeb05db + tests in work/slates-fluid*,
  -range*); main's §4.16 status still owes it; engine.rs differs on main, so it must be adapted.
- CANDIDATE 3 (large): CheckMerge / ReadMergeCheck / ResolveMerge with MergeStamp (work/slates-conflict-mutants,
  -atomic-conflicts, -conflict-cli); main has none; its paging parts likely duplicate A-267: verify first.
- Mounted Green/Work + extent sharing: owed by main's design (GAP-A9-14) but Codex's gates stayed red; a
  re-implementation on main, not a port.
- Not landable: measure/*, epoch-*, *-cost-*, *-profile-*, *-evidence, experiment/*, probe/* (no product change or
  measured-and-rejected).
- Pending verification against main: VFS journal derivation/exhaustion, db fixes, rt fixes, virtio-fs device-wake and
  device-id, FUSE invalidation/channel bounds, base/overlay descriptor leaks, pinned reads, recovery hardening,
  allocation-refusal families, NFS state binding, catalog ART staging, wire preflight.
- The analysis scratch under ~/.cache/slates-scratch/g081/ was lost in the wipe if it lived under ~/.cache; the refs
  remain in the clone (`git for-each-ref refs/codex`), so the segment tree can be rebuilt.

## Commits on main
- d0e39b52b A-494 (merge; 60cb771d7, b5fa5408f, 94f5c5759, 91954a02a, fadc81b76)

## Run requests filed
- 2026-11-19T00:55Z A-494: red-checks M1-M3, clippy + xtask, archive/api/vfs/server suites, Linux + xwin

## Decisions for Ada
(none)
