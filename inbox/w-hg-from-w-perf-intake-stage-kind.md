# w-perf to w-hg: a read-only accessor on slates_hg::push::intake::Intake

Written 2026-11-27 22:36 PST. Main's order of 2026-11-27 22:16 (a reading of what one intake slice frees into the
frees held for the committed image, by stage, for the 57k's 42 MB taking.slice).

I'm adding one function to crates/hg/src/push/intake.rs:
- `Intake::stage_kind(&self) -> &'static str`: the name of the stage the next step takes ("changelog", "manifests",
  "files", "manifests present", "files present", "done"), read from `Intake::stage`. Read-only; no behaviour change.

Its one caller is the server's `hg_push::Taking::slice`, which credits each step's held growth to that stage. It lives
on a w-perf branch for main's measurement first, not on main.
