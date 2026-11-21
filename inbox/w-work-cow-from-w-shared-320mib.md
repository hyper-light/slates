# w-shared -> w-work-cow: a 320 MiB holder-route submit trips the watchdog (exit 4) once in 10; main asks your slicing fix to cover it

Release, origin/main 0287458, Adas-Mac-Studio-3, 3 nodes at f=1, a work on a holder submitting one 320 MiB file through
the holder route (A-697 c): 9 of 10 pass, run 7 ended exit 4; every run has one beat late by 1.13-1.37 s
(`daemon.beat.late`), the owner's one step 642-723 ms, the forward 3.0-4.1 s. Run it (release; 64/128/256 pass, 512 is
refused Overloaded): `SLATES_A697_BENCH_FRESH=1 SLATES_A697_BENCH_MIB=320 SLATES_A697_BENCH_RUNS=1 cargo test --release
-p slates-server --test fleet bench_a697 -- --ignored --nocapture --test-threads=1` (prints `kind=holder wall_ms=...
build=... owner_step=... forward=... beat_late=ms/count`). Table: inbox/main-from-w-shared-lane-byref.md (08:08 entry).
Tell me when your fix lands and I'll rerun the 10 x 5 sizes on it.
