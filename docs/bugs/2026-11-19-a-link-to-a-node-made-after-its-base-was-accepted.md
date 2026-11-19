# A link to a node made at its target after the work's base was accepted (2026-11-19, found by g307's reading; fixed by A-676, g306)

## Description

A hard link a work makes names a node: the one at its target at the work's base, or the one the work makes itself
(A-646). Take a green whose version 1 makes a file `t` where nothing stood before, and a work based at version 0 that
links `l` to the node at `t`. Nothing stood at `t` at the work's base, so the link names no node the work saw, and
A-646's rule answers delete against modify, as it does for a target holding nothing now. The verdict accepted the link
instead, naming the file version 1 made. The same held for an IPC node (a FIFO) made at the target after the base.

No deriver makes such a link: `link(2)` answers `ENOENT` for a target that does not exist, and a target the work makes
itself is named as its own (`made`). So only a forged or hand-built increment reached the bug. g307 found it by reading
`Green::node_home` while fixing `hard_link_merges` (8d7209388).

## Root cause

`Green::merge_hardlink` resolves the link's node through `Green::node_home(target, base)`. For a target holding a file
or an IPC node now, `node_home` refused only one removed at the target after the base (`file_removed_since`, A-612 and
A-643's rule), and otherwise answered the target as the node's home. A node made at the target after the base was
never removed, so it passed, although nothing stood there at the base. `node_home`'s own doc comment stated the rule
("a path holding nothing is delete against modify, whether its node was removed since or none stood there at the base")
only for a target holding nothing now.

## Impact

A forged increment could make a link to a node its work never saw. For example, it could name a file another work made
after its base, and with A-646's node moves, take that file's node over when its home was later removed or replaced.
No real work's deriver makes the link, and no data was lost. The verdict accepted what A-646's rule refuses.

## Edits (A-676)

- `crates/merge/src/engine.rs`:
  - `Green::node_stood_at(path, base)`, new: whether a file or an IPC node stood at `path` at `base`. It is read off
    the histories by the one history rule and the copies (`content_len_at`, `special_at`), never by copying the file.
  - `Green::node_home` refuses a file or IPC node at the target that stood nowhere at the base, delete against modify,
    as it refuses one removed since. Its doc comment states the case.
- A copy replacing the target after the base needs nothing new: `remove_file_now` records the replaced file in
  `file_removed`, which `node_home` already refused.

## Tests

- `crates/merge/src/engine/node_tests.rs`, `a_link_to_a_node_made_after_its_base_conflicts`. A file, and a FIFO, are
  made at `made` after version 0, and each is linked by a work based at 0. Each link must be refused `DeleteModify`
  with no link made, and the test names every kind it misjudges.
- `crates/merge/src/engine/replaced_node_tests.rs`: `Work::fifo` makes a FIFO (the node `fifo_node` gives the fixture's
  `SPECIAL` as well).

macOS arm64 on the Mac Studio (kiln, debug, 2026-11-19):

- Red (20:05 UTC): `engine.rs` put back to its state before the fix (main effb30ac2) in a scratch clone. Running
  `cargo test --locked -p slates-merge --lib -- engine::node_tests::a_link_to_a_node_made_after_its_base_conflicts`
  failed, both links accepted (`a file: Accepted { version: 2 }; a FIFO: Accepted { version: 2 }`).
- Green over main effb30ac2: `cargo test --locked -p slates-merge --lib -- engine::node_tests
  engine::replaced_node_tests engine::restate_tests engine::stack` passed 62 of 62.
- Green over main 545181b30 (20:11 UTC): the same filters with `engine::compute_tests` passed 68 of 68,
  `cargo test --locked -p slates-merge --test engine -- hardlink hard_link` passed 6 of 6, and
  `cargo fmt -p slates-merge -- --check` was clean.

The other platforms, clippy and the whole suites are main's (CLAUDE.md §8).
