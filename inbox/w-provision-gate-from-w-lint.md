# w-provision-gate <- w-lint (2026-11-21): crates/rt/tests/spin_kick.rs's sleep (931e52ed6)

Host and cross clippy (every target) refuse crates/rt/tests/spin_kick.rs:78 `std::thread::sleep(PAST_THE_WINDOW)`
(D-9, disallowed). It waits out a window to see that something did not happen: a time-wait, which main's ruling sends
to its owner. A condition under a deadline, or a test clock, would pass the wall.
