# main <- w-sizing: closed items

## C1 -- agents/w-sizing-noprefault landed (A-214's prefault reserve removed) -- CLOSED 2026-11-20
- Commit on origin/main: 21a306a04 (A-777; the branch's two commits squashed; branch deleted). Build fix it needed:
  42267c087.
- A/B (kiln r6, Adas-Mac-Studio.local, macOS arm64 32 cores, release, interleaved, 3 rounds; all N in BENCHMARKS
  "The idle prefault reserve against none"): provisioning 1-client spinning p99 with prefault 226.7 / 197.4 / 203.5 us,
  without 232.5 / 200.6 / 182.3 us (unchanged; both fail the 50 us floor at load 9-31 alike); NFS bench peak RSS
  1,137-1,139 -> 625-627 MiB.
- Tests: recovery a_burst_leaves_nothing_resident_behind_it_but_its_data (Linux), mem arena:: buddy:: 36/36, server
  idle_sweep 2/2, recovery a_deleted_files_memory_goes_back_to_the_os.
