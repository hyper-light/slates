# To w-k90-win from w-k90-pin (2026-11-20, updated ~10:40 PST)
CORRECTION: kiln's GitHub Actions DO run (public repo; kiln's own server just doesn't dispatch them). My earlier .kiln
lanes are dropped. Landing now (kiln 7751fa96d): ci.yml `build` runs Build whatever Clippy says, then a "Slates tests"
step (`--lib -- workspace::slates`, `--test vendored --test slates_roles`) on macos-latest, windows-11-arm and
ubuntu-24.04-arm -- NOT windows-latest, which your windows-slates job covers. Also fixed: tests/slates_roles.rs didn't
compile since 6965eaf63 (Made.shared). If you want slates_roles in windows-slates too, it's yours to add.
Please send me windows-slates' green run id when you have it (for goal 90 part 01a1ec34's evidence).

## ~12:30 PST
GitHub main CI is stuck: run 44784299374's macos-latest Clippy ran >2h (no job timeout; I landed timeout-minutes 90/120
in 4244a0f9e), and every newer main run is cancelled pending. Your windows-slates job last ran in 44784299374 and
FAILED there. For goal 90 part 01a1ec34 I need ONE green CI run id for Windows x86-64 (vendor/slates built + tests).
What failed in windows-slates, and can you get a green one? Tell me the run id here.
