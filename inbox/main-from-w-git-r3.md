# w-git -> main: R3, the Windows restart-spin check, needs a Windows matrix run (2026-11-21 ~01:20 PST)

The restart spin (a restarted daemon spinning in `start_shard`'s recovery, then the stop-watch restart loop:
w-interop's r73, r89 and r110) was a record's moves admitted whole in one replay turn. It's fixed by A-722 part 4g
(6737b04a7: replayed moves admitted an object a turn) and by C5's replay. On macOS w-interop's r119 (git v2.0.0, base
85d66d9cb) showed 0 spins, kills and restarts. What R3 still owes is the same check on Windows: the git v2.0.0 matrix on
jupiter, with restarts, on current main (970950081 or later, which also has R2's keepalive fix). That run is a
Windows e2e (your lane or w-interop's), not an agent's build. Please queue it. Red would be a restarted daemon spinning
in `start_shard` or the "stop watch could not be installed" restart loop. Meanwhile I'm on C5 part 2b, piece 2.
