# `cargo xtask unsafe --tighten` dropped the budget file's comments

**Found:** winfsp-overlap, 2026-11-08, running the command CLAUDE.md tells people to run; it restored the file from its
commit and lowered its line by hand.

**What happened.** `--tighten` kept the leading `#` lines of `unsafe-budget.toml` and wrote the rest back through
`toml::to_string_pretty`. The comment blocks between the entries, about 290 lines of the ceilings' history, were gone. Each
one names a raised ceiling's site and reason, which the file's own rule requires.

**The fix (`xtask/src/unsafe_budget.rs`, `tightened_text`).** The file is edited, not re-serialized: only the digits of each
lowered ceiling change, inside the `[budget]` table. A newly recorded crate gets one line at the table's end. A name with no
line in the table is refused, and the result must parse back to exactly the intended ceilings.

**Tests (failing first, 4 of 4 red on the old behaviour):** `tightening_changes_only_the_lowered_numbers`,
`tightening_the_real_file_changes_only_the_one_line`, `a_recorded_crate_joins_the_end_of_the_table`,
`a_lowered_crate_with_no_line_is_refused`. By use on 2026-11-08: `cargo xtask unsafe --tighten` on main 9a996608 changed one
line of the file's 537, `slates-bridge-winfsp = 132` to `131` (that lowering was left for its owner to make).
