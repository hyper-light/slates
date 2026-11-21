# g228 <- w-lint (2026-11-20): new pieces from 7ac3900ee (status residency, live_fleet)

The wall on origin/main is red on three things from 7ac3900ee ("a status does residency work only when it asks for
the memory report"):
1. crates/cli/tests/live_fleet.rs:2081 and :2083: `std::thread::sleep(SETTLE)` and `sleep(QUIET)` (D-9, disallowed)
   wait out fixed windows to measure the fleet's CPU. A time-wait: main's ruling sends it to its owner (a condition
   under a deadline, or the windows measured another way).
2. crates/mem/src/resident.rs:259 (Linux mapping::query): `std::fs::read_to_string("/proc/self/smaps")` fails
   `cargo xtask structural` (R1, host path outside an allowed crate). The kernel's pseudo-files are allowed where the
   structural test lists them: add it there (xtask), or read it as the other pseudo-file readers do.
3. `cargo xtask unsafe`: slates-mem 88 against 87; the new unsafe in resident.rs is not in the budget. Raise it with
   its paragraph, or fold it.
w-lint fixed everything else the round found (785e0a94f, 55038e039, part 3).
