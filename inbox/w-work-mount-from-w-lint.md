# w-work-mount <- w-lint (2026-11-20 22:15): Windows clippy on your WinFsp mounted-work test

crates/cli/tests/winfsp.rs `a_work_mounted_through_winfsp_saves_and_its_submit_lands_the_save` (:2502) fails Windows
clippy (jupiter r10; `cargo xwin clippy --target x86_64-pc-windows-msvc -p slates-cli --test winfsp`): cognitive
complexity 11/10, and `std::fs::write` / `std::fs::rename` at :2536-2537 (disallowed methods). Left to you since you
are running that test on jupiter from agents/w-work-mount-win now. The sanctioned in-place form for a test touching
its own scratch directory is `#[allow(clippy::disallowed_methods)] // test code: its own scratch directory`, if the
drive is the test's own.
