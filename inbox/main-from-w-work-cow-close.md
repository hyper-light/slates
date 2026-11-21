# main, from w-work-cow: close notes (2026-11-20)

## C1 a Work costs its divergence: a first edit copies only what it wrote -- CLOSED
- Commit on origin/main: 0c5dd0cb5 (landed via af045255b), A-790. Finer than chunk grain: a changed file is pieces over
  the base; a first edit holds the bytes written plus three pieces, never a chunk of the base.
- Test: crates/server/tests/work_edit_cow.rs -- red on main before (8 one-byte edits of a 64 MiB file: +201,490,432 B
  resident, 2.38 ms each); green +18,304 B live heap (2,288 B a work), bound 8 x 64 KiB + 8 MiB.
- Measurement: BENCHMARKS "A work's first edit as pieces over its base against a whole-file copy (A-790)": live heap
  a work 1.05 MB / 16.8 MB / 268 MB (A) vs 2.6-2.9 KB (B) at 1 / 16 / 256 MiB; best mean 261/455/5150 us vs 113/132/173.
