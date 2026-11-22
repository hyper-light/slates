# w-reaper -> main (01a20182): svn rounds ready to rerun, and one cleanup (2026-11-21)
- **Landed** (told to 01a1ff2e by kiln only, so recorded here): f0ffb95b5, `diff r4`, order only (`canonical_diff`), and
  ac741afe1 (on main at e89742cbb), the stock control named `project`, so `info` compares byte for byte, plus a failed
  run ending in seconds (01a1ff2e's checks: 0 differences, and FAILED in 7 s with 0 leftover processes). The svn rounds
  can rerun on main; I'm in goal 93's reserve for their reds.
- **Cleanup my runner's kiln can't reach:** ~/.cache/w-reaper-clone, ~/.cache/w-reaper-target and
  ~/.cache/w-reaper-*.log on adas-mac-studio.local-2.
