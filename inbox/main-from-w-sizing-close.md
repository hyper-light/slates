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

## R2 -- 7.1 GB resident after an 84 MB push: content extents made on demand -- macOS + Linux CLOSED; Windows asked
- Commits on origin/main: A-789 362bd8c39 (macOS: each pool extent an object of its own, made ahead of need at its
  shard's start and at release, mapped by name, given back whole at release, adopted by name after a restart, released
  with a dead anchor); A-778 779840fba (a dead anchor's segment and content names removed); A-777 21a306a04 (the idle
  reserve that held the whole burst, 4,931 MiB, removed).
- Linux: no change needed. g226 measured one memfd + hole punch freeing the pages with a peer still mapping it
  (inbox/w-sizing-from-g226-giveback.md; BENCHMARKS "Content give-back per platform").
- Tests: recovery a_bursts_extents_go_back_to_the_system_once_its_volume_is_gone (red on adas-macbook-pro-2: 1,622
  MiB held after the volume was destroyed; green: 123); mem a_given_back_extent_is_made_again_empty_...; cli
  a_killed_anchors_segment_and_content_go_with_its_daemon (extents too); cli locked-volume SIGKILL-daemon restart
  byte for byte (recovery across processes); provisioning A/B r26 (BENCHMARKS; quiet rerun in run-requests).
- By use, hg's whole history (84 MB store) pushed over HTTP, kiln r35 on Adas-Mac-Studio.local (release; before =
  47dc24b72, after = 92f7a756b): daemon RSS after the push 6,198-6,200 MiB in both arms, unchanged after 20 s idle
  (r12 before the reserve's removal: the same 6.2 GB, and w-hg's run had 12,055 after idle with the refill). System
  free after the anchor's SIGKILL: after-arm 5,569 -> 11,951 MiB (memory back); before-arm stays 732 MiB (leaked; its
  two names were removed by hand). The 6.2 GB left is held content (w-hg's split: content store retained 4,495.5 MiB),
  which is R3's with w-hg and w-store, not a sizing reserve.
- Windows: g226 measured that only closing a section returns commit (decommit refused, discard per process). A
  section per extent needs the anchor to hold each extent's handle across a daemon restart (a new anchor<->daemon
  handle hand-off) and Windows builds/tests, which are main's. QUESTION: is Windows inside R2 (then I build it and
  put the Windows runs in run-requests), or a separate item?
