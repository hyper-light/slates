# A metadata change, or a link's rename, based before its node was replaced landed on the new one (2026-11-19, found by A-612's sweep; fixed by A-631; g267)

## Description

These are two siblings of A-612's defect. In both, a work changes a node it saw at its base. After that base the green
removes the node and makes another at its path, and then the work is submitted.

- A mode change, an extended attribute set, or an extended attribute removed: the verdict accepted it, and it changed
  the new node. Reproduced on the Mac Studio (kiln r786, red, 2026-11-19): the file `f` (mode 0o644, attribute
  `user.k = v`) was unlinked and made again. A mode change to 0o600 based before both was answered
  `Accepted { version: 3 }`, and the new file's metadata changed. The generated oracle shrank its failure to a symbolic
  link `l` removed and made again, then an attribute removal on `l` based before that, `Accepted { version: 3 }`.
- A rename of a symbolic link or of a hard link's name: the verdict moved the new link. On r786 the symlink `l` was
  removed and made again, and a rename of `l` to `moved` based before both was `Accepted { version: 3 }`. The new link
  left `l` and stood at `moved`.

## Root cause

A-612's record of removals (`Green::file_removed`) recorded regular files alone: `commit` wrote it for `RemoveContent`
and for a rename onto a file. The removal of a symbolic link (`RemoveSymlink`), of a hard link's name
(`RemoveHardlink`), of an IPC node (`remove_special`) or of a directory (`RemoveDir`) recorded nothing. So
`merge_rename`'s check, `file_removed_since(from, base)`, never saw a link's removal.

The metadata verdicts (`merge_setmode`, `merge_set_xattr`, `merge_remove_xattr`) judged a change by two things only: its
own dimension's last change (`mode_changed`, `xattr_changed`), and whether some node stood at the path at the head.
Removing a node does not touch its mode's or its attributes' last change, so to the verdict the work's change looked as
though nothing had happened since its base. The earlier guard for IPC nodes, `special_kind_changed`, caught only a
change of kind.

## Impact

A metadata change meant for one node was applied to another that the work never saw, and the client was told it was
accepted. That is a lost update against D-27. For a mode, it is a permission change landing on the wrong file; for an
attribute, a label or an access list moved onto another file. A rename moved a link the work never saw and took it off
its path. Holders recompute the same verdict, so nothing detected it. Exposed: any work based before a node's removal
and re-creation, whether an agent's work through the SDK or a mount, or a server-side merge of a review request.

## Edits (A-631, `crates/merge/src/engine.rs`)

- `commit` records `file_removed` for a removal of every type: in the `RemoveDir`, `RemoveSymlink` and `RemoveHardlink`
  arms, and in `remove_special`. A link's rename removes its source by `RemoveSymlink` or `RemoveHardlink`, so a link
  renamed away is recorded too. The record's doc says so.
- `merge_setmode`, `merge_set_xattr` and `merge_remove_xattr`: after the absent-path check, a path whose node was
  removed after the work's base answers `DeleteModify`, unless the work makes the path itself (`established`).
  `merge_remove_xattr` now takes `established`, as the other two did, and `decide_metadata` passes
  `r.establishes(path)`.
- `merge_rename` is unchanged: its A-612 check now reads the wider record.
- `file_removed_since`'s doc stands above it again. A-612 had left `intervening`'s doc on top of it.

## Tests

`crates/merge/src/engine/replaced_node_tests.rs`:
- `a_metadata_change_based_before_its_file_was_replaced_conflicts`: a mode change, an attribute set and an attribute
  removal, each `DeleteModify`, with the head unmoved and the metadata kept; a mode change based at the head is
  accepted;
- `a_rename_based_before_its_link_was_replaced_conflicts`: the symlink and the hard link, each `RenameRename` on the
  destination;
- `no_work_changes_a_node_removed_after_its_base`: generated histories over a file, a symlink, a hard link and a
  directory, each removed and made again while works based behind the head change a mode, an attribute or a name. Every
  answer is held to the rule, and a census requires each case to have run: a metadata change refused, a link's rename
  refused, a file's rename refused, and an accept.

Red on r786 with the edits reversed: all three failed. Green on r786: `cargo test -p slates-merge --lib` 79 of 79, and
`cargo fmt -p slates-merge -- --check` clean. Merged with main (r810): 92 of 92, and fmt clean.

## Siblings (reported, not changed; GAPS)

Probed on the Mac Studio with a scratch test that was never committed (2026-11-19, after r786), and probed again on
the tree merged with main (r810), where the first four still hold:
- A symbolic link or a hard link renamed over an unchanged regular file, even with the work based at the head:
  `Accepted { version: 1 }`, and the green then holds both the file (4 bytes) and the link at `f`. `merge_rename` emits
  the link at the destination and the source's removal, but no removal of the file it replaces.
- A directory's removal based before it was made again: `Accepted { version: 3 }`, and the new directory is gone.
  `merge_rmdir` takes no base.
- Three removals based before a metadata change at the head, each accepted: a directory's removal or a file's unlink
  before the head changed the mode, and a file's unlink before the head set an attribute. `merge_unlink` reads a mode
  or an attribute change only for an IPC node, and `merge_rmdir` reads neither.
- A hard link made to a target that was removed and made again after the work's base: accepted, naming the new file
  (13 bytes). `merge_hardlink` judges only the link's own path.
- A removed node's mode and attributes outlived it. After `f` was unlinked, the green still answered mode 0o644 and
  `user.k = v` for it, and a file made again at `f` showed both; a directory made again showed the removed one's 0o700.
  A-618 closed this on main meanwhile (`docs/bugs/2026-11-19-a-removed-files-mode-and-attributes-outlived-it.md`): on
  r810 neither is answered after a removal or a remake.
