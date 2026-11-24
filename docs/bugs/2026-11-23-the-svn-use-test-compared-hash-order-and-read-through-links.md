# The svn use test compared stock svn's hash order, and read a symbolic link through as a file (2026-11-23)

## Description
Main's round r202 (the big Studio, main 2e375de89), `a_dump_reads_and_writes_alike_through_slates_and_stock_svnserve`
over HTTP:
- **reads.dump:** 7 differences, all in `info`. At r1, r5, r6, r8, r9 and r11 (line 15) and r10 (line 25), stock
  listed `<entry kind="dir" path="trunk" ...>` where slates listed `tags` or `branches`: the same entries in another
  order.
- **git-2000.dump:** the harness panicked at `svn_interop.rs:2881:53`, `Os { code: 21, kind: IsADirectory }`.

## Root cause
Both are the harness's.
- **`info`:** `svn info -R` walks each directory's children in the order `svn_ra_get_dir2`'s hash iterates
  (Subversion 1.15 `libsvn_client/info.c` `push_dir_info`, unsorted). APR seeds every hash afresh
  (`tables/apr_hash.c` `apr_hash_make`: the clock, the pool's and the table's addresses), so stock's own order changes
  run to run. The comparison held it to one order, as it held `status` before w-svn's `entries_in_order`.
- **`IsADirectory`:** `tree_of` decided what to descend into by `DirEntry::file_type`, which does not follow a
  symbolic link, then read every other entry with `fs::read`, which does. git/git's history holds symbolic links,
  which stock `svn` checks out as links (`svn:special`), and one naming a directory was read as a file. Both legs check
  out the same links, so nothing in slates diverged.

## Impact
The use test over HTTP could not pass on any history with directories at a revision (`info`) or a link to a directory
(the working-copy reading). Nothing in slates was wrong.

## Edits
- `crates/cli/tests/svn_interop.rs`:
  - `info_entries_in_order`: a recursive info's entries compared in text order, what precedes and follows them as
    answered; the `info` verb compares through it.
  - `tree_of` holds a symbolic link as `link TARGET`, the text Subversion keeps for one, never reading through it.
  - Tests `an_infos_entry_order_is_no_part_of_an_answer` and `a_working_copys_symbolic_links_are_held_as_links` (red
    first, c5e705502).

## Evidence
- Before: r202, as quoted.
- After (the MacBook, release): the two tests and the comparison's other four, 6 of 6. Owed: main's rerun of (d) over
  HTTP with reads.dump and git-2000.dump.
