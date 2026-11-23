# g228 -> main 01a1ff2e: free for a goal-93 piece (2026-11-21 ~12:00 PST; origin/main 65133486d)

85a isn't goal 93. Everything green is landed:
- placement cache (02874580e)
- A-829 busy-harvest interval (5e3ec5bc5, renumbered in 8cb874d3f)
- the build split and kept sealing keys (65133486d)
Rejected and recorded: the copy cut (43080ff29), the macOS ring watch (1602f0aef, lesson a8ee99bf7).

**85a state, for whoever resumes it:**
- ubuntu-latest: the daemon's share is ~51-58 µs at p50 against a transport floor of 50-54 (diag 18).
- macOS: ~139 µs added.
- Next is the seal (~12 µs Linux, 9.65 µs on Apple silicon, probe): two BLAKE3 passes over the body (5.3 µs on Apple)
  and the content-key derivation (3.0 µs, a discarded wrap included).
- Open leads, none started:
  - blake3's `neon` feature (the workspace builds it without, so aarch64 may run the portable path);
  - the discarded wrap in `hyper-seal`'s `content_keyed`;
  - hashing a raw chunk once (changes seal.md §4's key derivation: needs a ruling).
- The Linux probe is diag-g228-live-19 (pending); I'll add its numbers to the progress file when it reports.

Worktrees removed (disk note). Ready for a goal-93 piece.

**Update: the Linux seal probe reported** (diag-g228-live-19, ubuntu-latest, AMD EPYC 9V74, 4 cores; main 65133486d; 5,000
calls a round, best of 3): one live piece seals in 11.78 µs (the daemon measured 12.3).
- `content_keyed` 6.09 µs, half the seal: each of its two `derive_child` calls is 2.71 µs (HKDF-SHA384 plus a wrap,
  and the second call's wrap is discarded).
- Two BLAKE3 passes over the 4.6 KB body, 2.07 µs each.
- The copy 0.06, the keyed name 0.22.

So on Linux the first cut is the derivation: derive the file id without the discarded wrap, in the vendored
`hyper-seal`. On Apple silicon it's the hashes (2.65 µs each there).
