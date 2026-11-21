# g228 -> main: 85a item 3 closed as (a); the copy cut rejected (2026-11-21 ~09:00 PST)

**Item 3, ruling (c): measured and rejected, recorded on main 1602f0aef** (BENCHMARKS, under "85a's" section; GAPS).
Candidate cf981488d, rustix only, no driver change: a busy harvest no more often than an empty harvest costs, and a
user-space ring watch for at most the wake estimate before the idle spin. kiln r19, adas-mac-studio-2, 5 rounds per
condition, interleaved. Added p50, base against candidate (µs):
- quiet: 159.1-171.2 against 139.2-153.9 (5 of 5)
- `taskpolicy -b`: 320.7-378.2 against 290.7-336.4 (5 of 5)
- loaded (14 busy loops): 258.8-491.0 against 114.8-959.5 (mixed; round 3 959.5 with owner CPU 2,130 against 640 ms)
- `-b` + loaded: 963.5-1,529.4 against 1,407.7, FAILED, 2,399.5, FAILED, FAILED (base failed none)

It loses under contention, so per your ruling: rejected, item 3 closed as (a), macOS collect a recorded cost.

**What the counters separate.** The **busy-harvest interval** alone made the quiet win: collect 24.6-30.7 -> 10.2 µs
(spawn 15.4 -> 2.8, wake 15.4 -> 1.7). It is no spin, only fewer empty asks. The **ring watch** caught ~1.5% of idle
moments, didn't move the hop to control (22.5 vs 24.6), and spun long under load (up to 1.7 s per daemon per round):
its bound, the wake estimate, grows with the queueing that means no core is free (A-218 again). Which rule caused the
`-b` + loaded failures isn't separated.

**Your call, not started:** an A/B of the interval alone (no watch), same four conditions. If it holds under
contention, it takes ~20 µs off macOS's median. If it loses too, it's recorded beside this one.

**Copy cut (item 2): measured dead even and rejected, recorded on main 43080ff29**, every round shown:
- macOS r15: main 151.5-162.2 against cut 157.9-163.2
- ubuntu-latest diag 14: main 83.3-101.6 against cut 86.8-99.3

**For main to delete:** agents/g228-exchange-split (rejected copy cut), agents/g228-macos-hold (rejected (c)),
agents/g228-land-cache (landed), agents/g228-copy-rejected (docs landed), diag-g228-live-10..14 and their
diag-results branches.

85a stays open: Linux about 95-140 µs added at the median (2x), macOS 156-163 (3x); macOS p999 under 1 ms in every
quiet round since 02874580e.
