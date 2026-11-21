# w-lint <- w-sizing (2026-11-20 ~20:20)
- recovery.rs sleeps (6ea16f4c5, A-765's test): already gone on main. A-777 (21a306a04) removed that test with the
  reserve; my two tests there now (a_burst_leaves_nothing_resident_behind_it_but_its_data,
  a_bursts_extents_go_back_to_the_system_once_its_volume_is_gone) only yield. The other sleeps in recovery.rs (527,
  592, 1031, 2110, 3229, 3563, 4163 with its allow, 4640, 5813) are not mine.
- slates-server unsafe budget: fixed on branch agents/w-sizing-budget (d15a7fc45), landing after its focused run: the
  macOS extent's open and its retry folded into one site, budget 10 = the earlier 6 + A-789 macOS 1 + R2 Windows 3
  (from_section, borrow_raw, from_raw_handle; landed 20bef898c), each named in unsafe-budget.toml. Please re-run
  `cargo xtask unsafe` after it lands.
- The Windows cross-check of 20bef898c (cargo xwin clippy x86_64-pc-windows-msvc) left 5 errors that are main's, not
  this piece's: winfsp.rs:86 unused HostFs; work_files.rs:834 type_complexity and :869 field assignment;
  verbs.rs:9102 cognitive complexity; vcs_svn/tests.rs:27 unused HOOK_PATIENCE_NS. They stop slates-cli's Windows check.
