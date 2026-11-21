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

## R2 Windows arm -- LANDED 2026-11-20 (main's ruling 18:40; fast landing 19:25)
- Commit on origin/main: 20bef898c (merge b30a0ab6d). Each content pool extent a section of its own on Windows, held by
  the anchor across a restart (new hold kind WHAT_CONTENT_EXTENT, bound class HoldBounds::extents = partitions x 64),
  made before any claim, closed when wholly free (commit back, g226's measurement).
- Mac focused tests on the landing commit (big Studio): anchor handles:: 18/18 (incl. new class test), mem shared:: 6/6,
  server content_pool config:: 11/11, api 3/3, fmt clean. Windows cross-check `cargo xwin clippy --target
  x86_64-pc-windows-msvc -- -D warnings`: clean in this piece's files; main's own 5 remain (winfsp.rs unused HostFs
  import; work_files.rs type_complexity + field assignment; verbs.rs assemble_post_state complexity; vcs_svn/tests.rs
  unused HOOK_PATIENCE_NS), which stop the cli crate's Windows check.
- Windows by-use (red at the base, green here, across processes): run-requests.md, for w-lint on jupiter.
- OPEN in R2 (found 19:45, in GAPS): the macOS give-back of a destroyed volume's burst depends on the machine --
  passed quiet (1,648 -> 123 MiB), kept 1,366 MiB on adas-macbook-pro-2 later and 335 MiB on the big Studio at 5,433
  free pages, at A-789's own commit too. Likely the idle release's keep (commitments + headroom + pressure hold) keeping
  the free extents. Next for me, before R3.

## R2 -- CLOSED on every platform (2026-11-20 ~20:10)
- macOS A-789 362bd8c39, Windows 20bef898c, Linux unchanged (g226's measurement), leak A-778 779840fba, reserve A-777.
- The variance found at 19:45 is not R2's: with the test's pressure readings injected (none / a fall of two bursts)
  the anchor's content object held 0-2 MiB after the destroyed burst, so the extents go back; the RAM that stayed (591
  / 340 MiB on the big Studio) is the process's own heap and buffers. Recorded in GAPS (c131361d3); worked under R3.
- Owed: the Windows by-use run in run-requests (w-lint, jupiter).
