# A hard link named a path and followed whatever was made there (2026-11-19, found by A-643's sweep; fixed in the engine by A-646 part 2a; g267)

## Description

The merge engine kept a hard link as its link path and its target's path. A file's other names did not survive the
file's removal, and a later file at the path became the links' file. Reproduced on the Mac Studio (kiln r1019, red,
2026-11-19), with `TARGET` (`g`) named by the links `h` and `second`:
- `g` removed: both links were left naming `g`, where nothing stood, and the bytes were gone. POSIX keeps a file's
  bytes while any name holds them.
- `g` removed and a new file made at `g`: `second` named the new file.
- `g` renamed to `moved`: both links still named `g`, the path the file had left.
- A file renamed over `g`: the links named the bytes renamed over the path, and `g`'s own bytes were gone.
- A FIFO named by a link could not be removed (`DeleteModify`), since the link would have named nothing.
- A link made to the link `h` named `h`, a link to a link, not the file.

## Root cause

A hard link was a namespace edge from one path to another (`hardlinks: link → target path`, `links_to: target path →
links`). Nothing moved it when its target's path was removed, renamed or replaced, and a link was resolved by its
target's path at the head, never by the node the work had linked to.

## Impact

Removing a file's first name destroyed a file its other names still held, against POSIX `unlink`. A link silently
became a name for another file. Renaming a file broke its links. An IPC node's names could not be cut down to one.
Exposed: any work that removes, renames or replaces a hard-linked file, and every link a work made to a link.

## Edits (A-646 part 2a, `crates/merge/src/engine.rs`)

- `Green::node_moves`, new, derived at the start of `commit` and never recorded. A node is its home, the one path
  holding its content or IPC node, plus the links naming it there.
  - When a version removes or replaces a home while links still name its node, the least surviving link becomes the
    home, taking the node's content or IPC node, mode and attributes; the other links name it there.
  - When a version renames a home, its links name the new home.

  `submit` records the verdict's effects before the commit, and `apply_recorded` runs the same commit, so a replay
  derives the same moves. A rebase restates only the work's effects.
- `merge_hardlink` resolves its target to a node's home (`node_home`). A link's home is used while that link has stood
  unchanged since the base, and a file's or IPC node's own path while it has not been removed since. The work's own
  node is reached through the links the work makes (`Resolved::link_node`). Any other target is refused:
  `DeleteModify` when the node is gone or has moved, `TypeChanged` when the path holds no file.
- An IPC node's removal is no longer refused for the links naming it; they keep it.

## Tests

`crates/merge/src/engine/node_tests.rs`: each case above, a link based before its target moved (refused, a guard), and
a replay test. In the replay test both greens' identities stay equal version by version, and each equals its recount
from scratch, so every key a move changes is marked. A-643's whole-state oracle now follows nodes: every link's home,
the hand-overs and re-points counted, and every link held to name a standing file or IPC node.

Red on r1019 with part 2a's engine change reversed: 9 failed. The oracle shrank to one removal of `TARGET`, leaving `h`
a link naming nothing. Green on r1019: `cargo test -p slates-merge --lib` 124 of 124, fmt clean.

## Owed (A-646 part 2b)

The ops document names a link's target by a path index, which the engine now reads as the node at that path at the
work's base, unless the work makes the target itself. The document is to say so: a node table, with each link naming a
node, made by the work or held at a path at its base, under a new format version with golden vectors re-derived.
