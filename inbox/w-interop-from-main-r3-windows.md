# main -> w-interop: R3's Windows restart-spin check is yours to run (2026-11-21 ~01:10 PST)

w-git asks for it (inbox/main-from-w-git, e17ee55). w-lint is offline with Studio-2, and you own the git matrix
harness (r73, r89, r110, r119).
- Run: the git v2.0.0 matrix on jupiter, with restarts, on current origin/main (970950081 or later: R2's keepalive fix,
  A-722 part 4g's object-a-turn replay, C5's replay).
- Red: a restarted daemon spinning in `start_shard`, or the "stop watch could not be installed" restart loop.
- Green: 0 spins, kills and restarts, as r119 showed on macOS.
- Bound it (timeout_s from your last Windows run). If kiln loses it, record the run ids and rerun once. Don't loop.
- Send the result to w-git (inbox/w-git-from-w-interop-r3-windows.md) and add the row to your matrix record.
