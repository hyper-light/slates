# g226 -> main: the residency fix (not goal 93), landing blocked on a machine (2026-11-21 11:10 PST)

- The fix b5220bd92 is green on macOS (Studio-4, 3/3: arenas 256 of 256 MiB, measured on the 2nd look, 76-86 ms).
  Merged with origin/main as 39321f431, plus a GAPS line (cd842b111). Not pushed yet.
- That merge auto-merged files the piece compiles against (crates/vfs/src/recover.rs, crates/server/src/state.rs,
  crates/ipc/src/protocol.rs, crates/server/tests/daemon.rs), so the fast loop wants the focused test once more. My
  target dir is gone and Studio-4 has 10 GiB free, so it goes through kiln (needs macos,mem=16g,disk=40g):
  - r9 failed: its Mac didn't answer the sync in 60 s.
  - r10 is queued: every Mac is busy, link down, barred for disk, or not answering.
- Linux r8 (adacachy, b5220bd92) is effectively stalled. rustc slates_server has had 36 s of CPU in 46 min while
  granted 8 CPUs (load 45). It is recorded as in flight in GAPS.
- Your call, one of:
  (a) I push 39321f431 + cd842b111 now: the merge had no conflict in the piece's own hunks, and you build after merges.
  (b) I wait for r10.
  Without an answer I wait for r10 and push when it is green, then write main-from-g226-free.md.

## 11:55 PST update
- Four macOS attempts at the focused rerun, all placed on d1323898 (adas-mac-studio.local-2), all ended before running:
  r9, r10 and r12 "did not answer the sync in 60s"; r13 "didn't answer place ... in 1920 ms". kiln listed every other Mac
  as link down, not answering, barred for disk, or full.
- I have stopped retrying. The branch stays at cd842b111 (b5220bd92 + merge 39321f431 + GAPS line), unpushed, in
  ~/slates-work/g226-live-shipped-volume on Studio-4.
- Your call, unchanged: (a) push now, with you building after the merge; or (b) I rerun when a Mac answers.
