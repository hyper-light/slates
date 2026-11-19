# A link renamed over a file left both at one path, and removals and links ignored their base (2026-11-19, found by A-631's sweep; fixed by A-643; g267)

## Description

These are four siblings of A-631, probed on the Mac Studio (kiln r810) and reproduced as failing tests (r874, r880,
2026-11-19).

1. A symbolic link or a hard link renamed over an unchanged regular file was accepted, even by a work based at the
   head, and the green then held both the file and the link at one path (`[File, Symlink]` at `f`). A later work
   based before the rename that renamed `f` away then moved the file's bytes out from under the link.
2. A directory's removal took no base. A work based before the directory was removed and made again removed the new
   one (`Accepted { version: 3 }`).
3. An unlink of a file, or a directory's removal, based before the head changed the node's mode or set an attribute,
   was accepted, and the change was lost with the node. For example, `f` with mode 0o700 and `user.k = v` set at the
   head was removed by a work based before both.
4. A hard link made to a file removed after the work's base was accepted. It named the file made in its place, or a
   path with nothing.

A fifth defect was found while fixing the first. Once a link renamed over a file replaced the file, a hard link
renamed over the very file it names removed the file's bytes and left the link naming itself (r880, red 2: `g` held
`[Hardlink]` and no bytes). Before A-643 the same rename had left `g` holding its file and a link to itself, with the
link's own path gone.

## Root cause

1. `merge_rename` moved a link to its destination with `Symlink`/`Hardlink` and removed the source, but emitted no
   removal of the file the destination held. A file renamed over a file needs none, since its `SetContent` overwrites
   the bytes; a link's set is another dimension and left the file in place. A-612's record of a rename onto a file
   lives in the content set, so the replaced file's removal was not recorded either.
2. `merge_rmdir` judged the current state alone: whether a directory stands and whether its children are emptied by
   the increment's own removals. It had no base to judge against.
3. `merge_unlink` judged a file by its bytes alone (`last_changed`); a mode or an attribute change is recorded in
   other dimensions. Only an IPC node's unlink read them. `merge_rmdir` read neither.
4. `merge_hardlink` judged the link's own path alone, never its target.
5. A hard link and the file it names are one file. POSIX `rename` does nothing when both names are links to one file,
   but the verdict treated the rename as a link replacing a different file.

## Impact

1. A path held two nodes: an inconsistent green whose bytes, link and identity disagree, with the file shadowed by
   the link. Any client could reach it with one `mv` of a link over a file through a work that names the rename alone.
2–3. A removal deleted a node another work had changed since the base, or a node made again since. That is a lost
   update against D-27, reported to the client as accepted.
4. A new link named a file the work never saw, or nothing.
5. A no-op `mv` of a hard link onto its own target destroyed the file's bytes.

Holders recompute the same verdict, so nothing detected any of these.

## Edits (A-643, `crates/merge/src/engine.rs`)

- `merge_rename`:
  - A symbolic link or a hard link renamed over a file emits `RemoveContent` for the destination, recording its
    removal, unless the increment removes that file itself by an unlink or a rename away. The paths the increment
    removes (`Resolved::cleared`) are now gathered once per verdict in `decide` and passed to the content and directory
    passes.
  - A hard link renamed over the file it names returns at once, with no effect, as a rename onto itself does.
- `merge_rmdir` takes the work's base and answers `DeleteModify` when the directory changed after it in any dimension,
  read off the change index (`changed_after`). That catches a directory made again and a mode or attribute change.
- `merge_unlink`: a regular file is judged by `changed_after`, a change in any dimension, where before only its bytes
  were judged.
- `merge_hardlink` answers `DeleteModify` on the link when its target was removed after the base (`file_removed_since`),
  unless the work makes the target itself (`Resolved::establishes`).

Metadata stays keyed by path (A-618). A link that replaces a file keeps the path's mode and attributes unless the work
changes them, as a file renamed over a file does.

## Tests

`crates/merge/src/engine/replaced_node_tests.rs`:
- `a_link_renamed_over_a_file_replaces_it`: the symlink and the hard link, each alone at `f` afterwards, and a rename
  of `f` based before refused `RenameRename`;
- `a_link_renamed_over_a_file_the_same_work_renamed_away_takes_its_place`: the file removed once, by its own rename;
- `a_hard_link_renamed_over_the_file_it_names_changes_nothing`;
- `a_directory_made_again_after_a_works_base_is_not_removed`;
- `a_removal_based_before_a_metadata_change_conflicts`: a file and a directory, after a mode change and after an
  attribute set;
- `a_hard_link_to_a_target_removed_after_its_base_conflicts`: made again and not, and the work's own target accepted;
- `the_green_stays_whole_and_no_work_removes_or_links_a_node_changed_after_its_base`: generated histories over two
  files, a symlink, a hard link and a directory. At the head, nodes are removed and made again and their metadata
  changed; works behind the head remove a node, link a file, or rename a link over either file. A model of what stands
  at every path at every version is compared with the green after every event, each answer is held to the rule, and a
  census of nine answers requires each to have run.

Red on r880 with all of A-643's engine change reversed: 6 of 7 failed (the rotation passes either way). With only the
same-file rule reversed, the same-file test and the oracle failed, and the oracle shrank to the hard link renamed over
its own target. Green on r880: `cargo test -p slates-merge --lib` 99 of 99, and `cargo fmt -p slates-merge -- --check`
clean.

## Siblings (reported, not changed; GAPS)

- A rename over a symbolic link, a hard link or an IPC node is refused `TypeChanged` even with no concurrent change
  (`occupied_by_nonfile`, read before any base check). POSIX `rename` replaces any non-directory destination, and does
  nothing for two links to one file, so `mv file link`, `mv link link` and a hard link renamed over another link to
  its own file can never land. These are false conflicts, not broken states. Found by reading.
- A hard link names its target by path. The head's removal of a file that hard links name is accepted (only an IPC
  node's removal reads its links), and the links then name a path with nothing, or the file made there next, where
  POSIX keeps the removed file's bytes under its remaining names. The oracle's model follows the engine here: its
  remake of `TARGET` is accepted while `HARDLINK` names it.
- `merge_unlink`'s IPC-node branch scans every attribute change and every hard link for each unlink
  (`xattr_changed.iter().any`, `hardlinks.values().any`), O(changes) per op; `changed_after` and `links_to` answer
  both by lookup. Found by reading.
