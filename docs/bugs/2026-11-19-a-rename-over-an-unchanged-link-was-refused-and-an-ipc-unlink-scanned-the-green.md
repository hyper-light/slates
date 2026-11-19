# A rename over an unchanged link or IPC node was refused, and an IPC node's unlink scanned the green (2026-11-19, found by A-643's sweep; fixed by A-646 part 1; g267)

## Description

1. A rename whose destination was a symbolic link, a hard link or an IPC node was refused `TypeChanged`, even by a
   work based at the head with nothing changed since. POSIX `rename` replaces any destination but a directory, so
   `mv file link`, `mv link link` and `mv file fifo` could never land. The same refusal met two names of one file
   renamed one over the other, such as a file over its own hard link, or one hard link over another link to the same
   file, which POSIX `rename` answers by doing nothing. Reproduced on the Mac Studio (kiln r960, red, 2026-11-19): the
   file renamed over the symlink, and the file renamed over its own hard link, were each answered `Conflict` with
   `TypeChanged`.
2. An IPC node's unlink scanned every attribute change and every hard link of the green, once per unlink. On r960
   (red), one unlink in a green of a thousand files, each with a hard link and an attribute change, examined 2,000
   entries.

## Root cause

1. `merge_rename` read `occupied_by_nonfile(dst)` before anything else, and answered any node but a file at the
   destination as a type change, whatever the work's base held. A type change is a conflict class for a concurrent
   change (§4.16 "The verdict"), not a refusal of an operation the work saw and made. A-643's rule for two names of
   one file covered only a hard link renamed over the file it names.
2. `merge_unlink`'s IPC branch judged the node by `xattr_changed.iter().any(...)` and
   `hardlinks.values().any(...)`, both walks of the whole map, although the change index (`latest_change`) answers a
   path's last change in any dimension by lookup, and the links' reverse map (`links_to`) answers which links name a
   path.

## Impact

1. A legitimate rename could never merge: the work's rebase met the same refusal, since nothing it could change made
   the destination a file. Exposed: any agent's `mv` over a link, a FIFO or a socket, and any client renaming one name
   of a file over another.
2. The cost was O(changes) per IPC node removed, on the verdict's path, which the green owner's shard runs for every
   submit.

## Edits (A-646 part 1, `crates/merge/src/engine.rs`)

- `merge_rename` asks `replaced_by_rename` what it replaces at the destination, as POSIX `rename` replaces any
  destination but a directory. A directory, or a link or IPC node changed after the base, is still `TypeChanged`. A
  file changed after the base is still `RenameRename`. An IPC node a hard link names is `DeleteModify`, as its unlink
  is. An unchanged link or IPC node is removed by its own effect, pushed before the source's set, so the history's last
  entry at the version is the new node. The A-643 replacement of a file by a link is folded into the same function.
- `same_file` answers two names of one file: a hard link and the file it names, or two links to one file. A rename of
  one over the other changes nothing.
- `merge_unlink`'s IPC branch reads `changed_after(path, base)` and `named_by_links(path)`: two lookups.

## Tests

- `crates/merge/src/engine/replaced_node_tests.rs`:
  - `a_rename_over_an_unchanged_link_or_ipc_node_replaces_it`: the file and the symlink, each over a symlink, a hard
    link and a FIFO;
  - `a_rename_over_a_link_changed_after_its_base_or_a_directory_is_refused`: the guard, which passes either way;
  - `two_names_of_one_file_renamed_one_over_the_other_change_nothing`;
  - A-643's whole-state oracle now renames the file or a link over a file or a link, with a census count of a link
    replaced.
- `crates/merge/src/engine/scan_tests.rs`: a counter of the entries the verdict examines, kept in test builds. One IPC
  node's removal examines two entries in a green of a thousand hard links and attribute changes. A work based before an
  attribute change on the node is refused.

Red on r960 with the old rename rule and the old scans restored, the scans counting: 4 of the 7 tests the run selected
failed (the guards pass either way), and the removal examined 2,000 entries. Green on r960: `cargo test -p slates-merge
--lib` 104 of 104, fmt clean; merged with main (r972), 116 of 116.
