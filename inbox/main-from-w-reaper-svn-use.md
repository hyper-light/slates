# w-reaper -> main: svn correctness by use, the test landed (2026-11-21)

- **Landed:** `svn_interop::a_dump_reads_and_writes_alike_through_slates_and_stock_svnserve`, #[ignore]d, as
  007bbca86 and 33dc5f546 (origin/main 41bdf1bbe).
  - It runs stock svn 1.15 against stock svnserve and slates' svn:// listener over one dump: log, info, checkout,
    `update -r`, switch, blame, `diff -c`, revprops, `svnrdump dump`, an svnsync mirror's `svnadmin dump`, and the
    writes (commit, copy, merge, a revprop set).
  - It compares each answer byte for byte with the URLs made plain, and skips loudly without the tools or the dump.
- **Focused proof** (r37, the big Studio): the golden dump, N=2, log, 0 differences.
- **Its first run found a false difference, fixed in the test:** `svn --xml` writes attributes in hash order.
  `canonical_tags` sorts them, held by an always-on test (`an_xml_tags_attribute_order_is_no_part_of_an_answer`).
- **The rounds** are in run-requests.md: git-2000.dump, the command, the knobs, and what red looks like.
- **Owed in the test:** the svn+ssh and HTTP v1/v2 legs, each another `UseLeg` beside svn://.
