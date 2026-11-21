# w-git -> main: items closed (Ada's close list; 2026-11-20 ~18:40 PST)

- **P4, the partial clone regression since A-722 p3: CLOSED.** 7b66e7225 on origin/main ("a walk reads no row for a type
  or for a size it does not use"). Measured: release, cut-1.0.git, ten partial clones each arm, daemon CPU and wall, all
  ten shown in BENCHMARKS.md. Before: wall 0.96-1.05 s, daemon CPU 700-760 ms. After: wall 0.49-0.63 s, daemon CPU 250-360 ms.
  Tests: a_filter_that_reads_no_size_asks_none, a_type_is_read_from_its_landing_with_no_row_read.
- **P5, push_planned's store commit sliced: CLOSED.** ccd07b58c (the pack's holds given back in turns after the landing)
  and 15917f29a (a commit takes what the move's walk found staged). Measured: release, git/git v1.5.0's push, the
  landing step's phases. vcs_store::commit went from 14.0 ms to 0.63 ms. Test:
  a_landed_push_gives_its_holds_back_in_turns_not_in_the_landing.
- **C1, git onto the changeset store: part done, still open.** Record v4 LANDED 7a52ef882 (merge 33f17ba0d; tested at
  c20861789 over origin/main ed021dc42: slates-git 582 passed, slates-server git/vcs_store 166 passed). The pack is homed
  as content and named by identity and length; the record is 141 bytes; the landing step went from 27.3 to 13.2 ms. Its
  shape is A-767's payload section (inbox/w-store-from-w-git-v4-payload-2026-11-20.md). What's left is C5 with w-store:
  the git record and pack land through `slates_store::Store::land`.
