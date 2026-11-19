# A removed file's mode and attributes outlived it (2026-11-19, A-618)

## Description

Found by A-618's client-fidelity test (§4.17.1 "A push is a work" item 4), which after every accepted push of
generated histories checks that the green holds exactly the pushed tree. A history made `a/b/f` an executable file,
removed it in a later push, and made it again as a plain file in a third: the green reported `a/b/f` with mode 0755.
The same holds for a file's attributes and a directory's mode and attributes: a path removed in one version and made
again in a later one took the removed one's metadata. A focused test shows it with the engine alone: land an executable
file with an attribute and a directory with a mode, remove both, and the green still answers the mode and the attribute
at both paths.

## Root cause

The engine keeps a path's mode and attributes in maps of their own, keyed by path, beside its content (§4.16 "The
state's maps"). Removing a special node drops them with it (`Green::remove_special`: "its mode and attributes go with
it"), but committing a removed file (`Effect::RemoveContent`) or a removed directory (`Effect::RemoveDir`) touched only
the content or the directory, so their metadata stayed under a path nothing stood at. A file or directory made there in
a later version had no `SetMode` of its own when its maker's default was the green's (a push's plain file, which checks
out 0644 when the green records no mode) and took the stale value; its attributes came back the same way.

## Impact

A git or Mercurial client that deleted a file and later pushed it again as a plain file would see the server's checkout
of that branch make it executable, against item 4 (every path holds exactly what the client sent). An agent's work
through a mount that removed a file and made a new one at its path met the old file's attributes on it. The green's
head identity counted the stale entries, so two greens holding the same files could differ in identity.

## Edits

- `crates/merge/src/engine.rs`: `commit` stages the paths a version's `RemoveContent`, `RemoveDir`, `RemoveSymlink` and
  `RemoveHardlink` effects remove and, after every effect is applied, drops the mode and attributes of each at which
  nothing then stands (`holds`: no file, directory, special node, symbolic link or hard link), so a rename or a file
  made again within the version keeps them. The dropping is `drop_metadata`, the code `remove_special` held, which
  `remove_special` now calls: the history keeps the values, and the identity and the verdict's change index see the
  change.
- Test: `a_removed_file_and_directory_take_their_mode_and_attributes_with_them` (`crates/merge/src/push/tests.rs`), red
  without the change (the removed paths answered mode 0755, the attribute, and mode 0700); and the client-fidelity test
  over generated histories (`every_push_is_the_oracles_work_and_leaves_the_client_where_it_is`), which first met it.
- Siblings swept: a removed symbolic link or hard link (`RemoveSymlink`, `RemoveHardlink`) now drops its path's metadata
  the same way. A work cannot give a link a mode or an attribute (the deriver refuses either off a file or a
  directory), but an origin seeded from a host directory can hold one; a special node already dropped its own. None
  open.
