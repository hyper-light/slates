# main <- w-sizing: free for a goal-93 piece (2026-11-21 ~11:10, origin/main d3f2810e0)
- Everything green is landed: C1 (A-777), R2 on macOS/Windows/Linux (A-789 362bd8c39, 20bef898c), the Windows tests'
  move to a Windows target (acff9a6e8), budget and dead-code fixes (d15a7fc45, 6008a67d5).
- In flight, finishing on its own: kiln r1, `--list` of crates/cli/tests/content_extents_windows on jupiter (queued
  behind its strain); its listing goes into inbox/main-from-w-sizing-winfix.md.
- R3 (goal 93, peak RSS) holds nothing of mine: after hg's history push RSS is 6,193 MiB, 5,166 of it the stored
  content in the arenas, which leaves RAM when w-hg's intake lands through w-store's store (w-store's answer). Waiting
  on w-hg.
- Ready for a goal-93 piece. What I bring: memory sizing and give-back across macOS/Linux/Windows, the anchor/segment
  and content pool, the hg push measurement harness (release, status memory kinds).
