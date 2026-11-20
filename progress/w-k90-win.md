# w-k90-win progress (kiln goal 90, Windows parts)

## 2026-11-20 ~09:30 PST start (Adas-Mac-Studio-2, b24c; pushes over SSH)
- State folder: ~/fanout-state-w-k90-win-s (shallow https clone, push over ssh). The full SSH clone at
  ~/fanout-state-w-k90-win timed out half-made (only .git): for main to delete.
- Also for main to delete: ~/kiln-work/w-k90-win-kiln (blobless kiln clone whose checkout failed; promisor fetch broken).
- kiln worktree: ~/kiln-work/w-k90-win (branch agents/w-k90-win, from ~/Projects/kiln, fetched over https), base ed8991d29.
- Found: the bare_metal drive's Windows code already landed (b7720ebca, cfg(any(unix, windows)), "built by nothing
  here"). Part 1 is now: build it on Windows, run it (needs WinFsp + Python 3), fix what breaks.
- kiln GitHub CI on ed8991d29 (run 44758164143): windows-latest / windows-11-arm Clippy fail (windows-latest exit 101 =
  compile error); every lane's clippy fails too. Windows lanes only clippy+build; no Windows test lane runs kiln's tests.
- Plan: (a) jupiter: cargo build + `cargo test --no-run` of kiln (AWS_LC_SYS_PREBUILT_NASM=1, own target off C: if a
  drive has more room); fix Windows compile errors; run the no-mount tests (slates_e2e checks, workspace libs).
  (b) GitHub CI: a Windows test lane installing WinFsp (as slates' CI does) running slates_e2e bare_metal with
  SLATES_E2E=1, plus kiln's Windows-relevant tests.
