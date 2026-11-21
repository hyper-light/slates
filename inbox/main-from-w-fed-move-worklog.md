# main <- w-fed-move: the 300 MB work log, closed (A-806, Slates 4c404823; commit dc0adee7), 2026-11-21 ~03:15 PST

## The item
One work's log grew with the file it touched, not with what it wrote: unbounded growth (CLAUDE.md §2 item 8).

## Named
New peak counter: `merge.work.log_peak_bytes`, `_entries`. In the 1,000-client stand-in:
- over `kernel/`, the largest log was 752,530 bytes in 7 entries;
- over `asic_reg` (512 headers of about 1 MB), 35,249,811 bytes in 8 entries.
Both for 64-byte edits. The cause: a rebase answered each changed file whole, the work held it whole, and its
restatement logged it whole.

## Fixed
A rebase now answers each changed file as parts over the head (the A-790 pieces). The work holds them and its
restatement logs them, so a work's log is what it wrote.

## Tests
- Red first: `a_rebased_works_log_holds_what_it_wrote_not_the_file` (`--test work_edit_cow`). A 64-byte write into a
  1 MiB file, then a rebase: 1,048,635 bytes before, under 1,088 after.
- The parts oracle: `a_rebased_files_parts_read_back_as_the_file` (generated edits read back exactly).
- Passing: the rebase-touching server targets (`work_cow`, `mounted_work_cow`, `merge_load`, `green_post_state`,
  `verdict_pages`, `works_off_owner`), the fleet's rebase/check/merge tests, and the lib's work and verbs tests.

## A/B
Largest work log, oracle clean both ways:
- `kernel/`: 752,530 to 2,036 bytes;
- `asic_reg`: 35,249,811 to 1,957 bytes.
Verdicts/s on `asic_reg` were not comparable here: load average 8 and swap at 96 of 97 GB from other agents' work.

## Sibling finding, not mine, untouched
`daemon`'s `the_merge_service_enforces_roles_pins_versions_and_seals_behind_the_barrier` fails on main. It asserts a
green's landing is refused `Unsupported`, but A-774 part 3 (f401f798c) made a green landable, so the test is stale.

## Next
r48's rows when they come; option (1) of (b) is w-rewrite-ops'.
