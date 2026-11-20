# A file's mode was lost when it was renamed (2026-11-20, w-shared, UI goal 93)

- **Seen by use:** in a work on a git repository's green, `create d/a; chmod 0755 d/a; mkdir x; mv d/a x/a`, then
  submit: refused `BadRequest` ("increment does not compose: SetModeMissing(\"d/a\")"). With the chmod and the mv in
  two versions the move landed and a stock clone listed `x/a` `100644`, where POSIX `rename` and `git mv` keep `0755`.
- **Cause:** `slates_merge::increment::compose_modes` keyed each mode by the path it was set at and never followed a
  `Rename`, and the green keeps a mode at its path (A-618), so the mode stayed behind at the old name.
- **Fix:** `compose_modes` walks the journal in order and moves a file's mode with each rename: a mode set before it,
  or the base file's own mode, is declared at the new path.
- **Tests:** `crates/merge/tests/increment.rs` (`set_mode_then_rename_sets_the_mode_at_the_new_path`,
  `a_new_executable_file_moved_keeps_its_mode`, `a_base_files_mode_moves_with_its_rename`); by use,
  `vcs_store::commits::tests::t_a664_p3_an_executable_file_moved_is_100755_in_its_commit` (a stock-protocol clone lists
  both moved files `100755`).
