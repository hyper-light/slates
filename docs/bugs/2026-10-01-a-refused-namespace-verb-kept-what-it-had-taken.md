# A refused namespace verb kept what it had taken

**Date:** 2026-10-01. **Area:** `slates-vfs` (`volume.rs`, `dir.rs`, `dirtree.rs`, `trie.rs`, `base.rs`),
`slates-mem` (`slab.rs`). **Audit:** AUD-29-40 (P1). **Design:** §4.2, §4.5, T-1.9, A-60.

## Description

The audit's reproduction: on a volume with an inode allowance of four and an entry allowance of zero, three
refused creates raised the inode usage from (1, 4) to (4, 4), with no new name. Once the entry allowance was
raised, the volume could create nothing.

Injecting a refusal at every allocation step of every namespace verb found worse defects than the charge:

1. **The inode charge (the audit's case).** `create_leaf`, `mkdir` and `symlink` issued and installed the
   inode, then let `dir_insert` refuse it, without undoing the install or the charge.
2. **A tree split dropped entries.** `insert_split` moved the upper half of a full block into a sibling,
   then allocated the sibling's slot. On a full block slab that refusal discarded the moved half: existing
   names vanished.
3. **The inode table leaked a copied path.** `trie::set` and `trie::remove` copied the path node by node.
   A refusal partway left copies no root reached; a copied root was lost.
4. **A small directory's move to a tree** kept the blocks it had taken when it was refused partway.
5. **A directory copy-up orphaned its copy.** `make_current_dir_node` inserted the node's copy, then
   copied its inode. When the inode copy was refused, the node copy was unreachable, and each retry leaked
   another.
6. **An inode copy-up was half-done.** `make_current_inode` charged retention and moved the inline bytes'
   accounting into the head's unique bytes, then had its version slot refused.
7. **A rename lost a file.** It removed the target, then the source, then copied up the moved object and
   inserted it. A refusal after the removals left neither name, and a `/g` renamed across directories at
   the inode bound disappeared.
8. **The overlay witnessed before the core refused.** For a base file, the overlay's `link` and `rename`
   journaled a `Witness` record and added a diverged entry before the core verb was refused.

## Root cause

Each verb interleaved fallible allocations with visible changes. Admission was checked by whichever step
happened to allocate, after earlier steps had already changed something. Copy-up was treated as invisible
preparation, but it moves inline bytes into the head's unique accounting and spends slab slots.

## Fix

- **Admission before mutation, for the whole verb.** Each namespace verb (create, mknod, mkdir, symlink,
  link, rename) counts, before anything changes, every slot it can take:
  - the copy-ups of its directories' paths (node, inode version, trie path, the parent's entry-tree
    copies) and of the objects it touches;
  - its own new inode and directory node;
  - the blocks of its entry changes;
  - the retention its copy-ups charge.

  It admits them whole against each slab's exact room (`Slab::room`) and the retention budgets, or refuses
  having changed nothing (`Needs`, `admit_needs`).
- **Worst-case counts.** The split's worst case (a sibling per level and a new root) and trie paths that
  later copies share are counted at their worst. This is a worst-case transaction reservation, as XFS
  reserves log space; at most a few slots are refused early.
- **Each structure all or nothing on its own:**
  - the trie's `set` and `remove`;
  - the tree's `insert`, `remove`, `set_child` and a new `respell`, which admit their copies and splits
    first;
  - the small-to-tree move, which builds aside and discards on refusal;
  - `make_current_dir_node`, reordered so each step is valid preparation or undone;
  - `make_current_inode`, which moves accounting only after its version is installed and returns its
    retention charge on refusal.
- **Rename is prepare, publish, consequences.**
  - Every copy-up happens first.
  - The new name is published: the replaced entry is re-pointed in place, or the name is placed.
  - The old name is removed; if that is refused, the first step is undone.
  - A respelling of one entry is one in-place step.
- **The overlay asks first.** It runs the core's `admit_link` and `admit_rename` before witnessing a base
  source.
- **Charges are unwound.** A create's inode is uninstalled and its charge returned if its name cannot be
  published.

## Tests

- **`crates/vfs/tests/namespace_refusal.rs`:**
  - `a_namespace_verb_refused_at_any_allocation_changes_nothing`: nine verbs × three slab dimensions × with
    and without a snapshot, with the cap raised a slot at a time from the scenario's own use.
  - `a_verb_on_base_entries_refused_at_any_allocation_changes_nothing`: the same over an overlay's disk
    entries (a base file linked or renamed, a base directory renamed, names created in merged
    directories).
  - Each refusal must be typed, and must leave the following as they were: every path's kind, attributes
    and contents; inode and entry usage; accounting; the journal; the diverged set; and every slab's
    usage. A repeat must refuse the same way, and the verb must succeed once the dimension has room.
  - 44 refusals were injected on the scratch scenario and 46 on base entries (2026-10-01).
  - `a_create_refused_for_its_entry_returns_its_inode_charge` (the audit's reproduction) and
    `a_verb_refused_by_an_allowance_changes_nothing_and_succeeds_with_room`.
- **Before the fix:** all three of the first tests failed. The sweep met the defects in the order listed
  above, each named by its first failure message (for example, `inodes (307, …) -> (308, …)`, and `- /g
  File …` for the lost file).
- **Unit tests:**
  - `slab::room_counts_exactly_the_inserts_that_succeed`, which also passes under Miri;
  - `dir::a_small_directory_moves_to_a_tree_in_one_block`, the derivation of `SMALL_TO_TREE_BLOCKS`.
- **Unchanged elsewhere:**
  - the vfs suite (26 binaries);
  - `slates-bridge-nfs`, `-land`, `-base`, `-bridge-core`: 252 passed;
  - the server library: 147;
  - the CLI suite with a live kernel mount: 13.

## Siblings reported

- **Other verbs are not swept yet.** Unlink, rmdir, setattr, write, truncate and the extended-attribute
  verbs still interleave copy-ups and changes. They are the next sweep under the same harness.
- **An overlay read copies up after a snapshot.** `follow_live_disk` makes a base inode current to record
  what it observed, so a `stat` after a snapshot spends a version slot and can be refused at a full slab.
- **A split's `InvalidName` refusal comes after the split.** It is unreachable for names within `NAME_MAX`
  (a split leaves each side room for one), but it is a refusal after a change.
- **Rename's consequences can still fail after publication.** If releasing a replaced directory's node,
  or dropping a replaced file's last link, fails, the error reaches the caller after the names have moved.
  These steps allocate nothing now; their remaining failure modes are typed lookups.
