# main, from w-work-cow: close notes (2026-11-20)

## C1 a Work costs its divergence: a first edit copies only what it wrote -- CLOSED
- Commit on origin/main: 0c5dd0cb5 (landed via af045255b), A-790. Finer than chunk grain: a changed file is pieces over
  the base; a first edit holds the bytes written plus three pieces, never a chunk of the base.
- Test: crates/server/tests/work_edit_cow.rs -- red on main before (8 one-byte edits of a 64 MiB file: +201,490,432 B
  resident, 2.38 ms each); green +18,304 B live heap (2,288 B a work), bound 8 x 64 KiB + 8 MiB.
- Measurement: BENCHMARKS "A work's first edit as pieces over its base against a whole-file copy (A-790)": live heap
  a work 1.05 MB / 16.8 MB / 268 MB (A) vs 2.6-2.9 KB (B) at 1 / 16 / 256 MiB; best mean 261/455/5150 us vs 113/132/173.

## P2 the brief's deciding measurement over the grid -- CLOSED
- Commit on origin/main: 5aef5f02f (landed via f9c113ae9).
- Measurement: BENCHMARKS "A work over a green costs its divergence: the brief's grid, before and after (w-work-cow
  P2)": green 1 MiB / 32 MiB / 1 GiB x files 1k / 10k / 100k x works 1 / 10 / 100, three rounds, release, before and
  after interleaved, 135 runs all shown. After: 1.6-5.6 KB of live heap a work at every point, flat in green size and
  works (O(N x metadata)). Before (a copy of the green per work, run where N x G <= 2 GiB): 1.5 MB to 1.25 GB a work,
  time to 129 ms. Test: crates/server/tests/work_cow.rs (bounds live heap and resident).
- Finding (GAPS, not worked): one submit of a 1 GiB or 100k-file changeset holds its shard 1,097 ms past its
  1,025 ms window; the watchdog ends the daemon. The grid seeds in parts of 16 MiB / 5,000 files.
