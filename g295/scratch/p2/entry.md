### A-626 part 2 — A copy from a version its source changed after brings the tree as it stood then: the head's graft fixed by what the change index names since, the fix recorded whole in the chain entry, its room derived before the verdict (2026-11-19, g295 for UI goal 93; §4.16 "The state's maps" and "The verdict", §4.17.4 item 3 and piece 3; A-586, A-626 part 1)
Applied in the same change to: §4.16 (a status block, the verdict's sentence on `Copy`, the "Copies" bullet), §4.17.4
(a status block, piece 3), the A-626 part 1 entry (its owed list), GAPS (the §4.17.4 entry),
`crates/merge/src/chain_entry.rs` (`CopyFix`, `COPY_TREE_FIXED`, `fix_bytes`), `crates/merge/src/engine/copy.rs`
(`copy_fix`, `SourceAt`, `copy_entry_room`, `EntryCopies`, the fix's commit), `crates/merge/src/engine.rs`
(`Effect::CopyTree`'s fix, the verdict's fixes, replay), `crates/merge/src/dims/copy.rs` and `crates/merge/src/ptrie.rs`
(`count_tree`, `Ptrie::prefix_len`), `crates/server/src/verbs.rs` (the entry's bound), and the tests
(`engine/copy_tests.rs`, `chain_entry.rs`).
- **What was there.** A copy whose source's tree changed after the version it names was refused (`RenameRename`). FSFS
  takes it, since a node-revision is kept whole forever: `svn cp ^/trunk@100 ^/branches/old` with the head at 5,000, a
  working copy's `svn cp -r 100`, a merge or a revert that restores an old tree.
- **How the tree at the version is built.** The maps hold the head; the histories hold the rest; and the change index
  names exactly the paths that changed since a version. So the copy grafts the head's tree and fixes it (`CopyFix`):
  each key at a path the change index names in the source's tree since the version is set to its value then, where it
  differs from what the graft brings; a copy made since inside the source is cut (its tree was absent or taken away at
  its version, so every key it held at the version is named by its removal); a copy made since over the source replaced
  the whole tree, so nothing of the head's is grafted (`start_empty`) and every key the tree held then is set. The cost
  is the change since the version, O(the change index's entries since and the keys at the paths they name), never the
  tree. A key the head holds but no path names is unchanged since the version, and an attribute name with no history of
  its own reads alike at both ends, so the head's names are never listed: the copy still walks no map.
- **The commit and the identity.** The fix is applied after the graft, its values set with nothing recorded key by
  key: the copy's entry in the set hash already stands for the source's tree at the version it names (the recount
  reads `copied_tree_entries` there), and a key's first change after the copy records the value the copy gave it, as
  A-586 rules. The readers need nothing new.
- **The verdict's view.** The copied tree's operations in the same increment (edits, creations, removals, directories,
  metadata, a nested copy's place) are judged against the source as the copy brings it (`SourceAt`: the head's keys,
  less the fix's cuts, with its values set), so an edit beneath a copy of an older version splices into that version's
  bytes. Whether anything stood in a tree at the version, which a copy needs (FSFS refuses a missing source), is
  counted, never walked: the radix trees keep each node's count (`Ptrie::prefix_len`, `count_tree`), so the head's keys
  under the root, less the cut subtrees', plus each key the fix sets and less each it takes, is a number of descents.
- **The chain entry.** A fixed copy is the record kind `COPY_TREE_FIXED` (19): `COPY_TREE`'s two paths, then the fix
  (whether it starts empty, then its cuts, files, IPC nodes, directories, modes, symbolic links and attributes, each a
  count and its records in key order, a value a presence byte and its bytes). Every count is checked against the bytes
  left before anything is allocated, a key written twice is refused (`DuplicatePath`), a flag other than 0 or 1 is
  refused (`BadKind`). A copy without a fix keeps tag 18's bytes, so every entry written before reads as it did. Replay
  applies the recorded fix, and an edit beneath a fixed copy reads the bytes the copy brings from the entry
  (`EntryCopies`): it reads no history, whatever its holder folded or restored.
- **The room, derived.** `Green::copy_entry_room(inc)` is the sum of `chain_entry::fix_bytes` over the fixes the
  increment's copies need, computed from the green's histories before the verdict; the service adds it to
  `entry_bound`, which counts the increment alone (`crates/server/src/verbs.rs`), so the chain admits what the entry
  will hold. `fix_bytes` equals the encoding's length (a test holds it), and a copy of an unchanged source needs none.
- **Refusals.** A version whose values are folded away (below the fold floor, or the checkpoint the green was restored
  at) is refused for a source changed since (`TypeChanged`: what it held is gone); a source unchanged since is copied
  from any version, the head's tree being the version's. A hard link in the tree at the version is refused as one at
  the head is.
- **Tests** (`crates/merge/src/engine/copy_tests.rs`, `crates/merge/src/chain_entry.rs`).
  - A copy of the trunk at version 1 after two versions changed it in every dimension holds the trunk's tree at 1;
    the change index names its destination alone; the entry fits the room derived for it, and a copy of an unchanged
    source needs none; it replays from its encoded entry.
  - Copies from before a copy inside the source (cut) and from before one over it (start empty), replayed version by
    version.
  - An edit beneath a copy of an older version in the copy's own increment lands on that version's bytes, live and
    replayed.
  - A copy from a folded version of a changed source is refused typed; one of an unchanged source is accepted.
  - The fix's codec: a round trip with `fix_bytes` its length, a golden vector, and hostile input (a count past the
    bytes, a key twice, a bad flag) refused typed.
  - The oracle's model now copies each source as its version held it; its census counts copies accepted from a
    version their source changed after.
- **Evidence** (the Mac Studio M3 Ultra, kiln, debug, fresh clones of `agents/g295-a626-part2`; 2026-11-19).
  - The first run (`69936d04d`, 06:34 UTC, load 12.2): 117 of 118; the oracle found the copy walking a map, the head's
    attribute names listed at each named path; read from the histories alone since (`4ff0b7131`).
  - `cargo test --locked -p slates-merge --lib` at `4ff0b7131` (r1027, 06:38 UTC, load 9.4): 118 of 118, 24.40 s;
    `cargo fmt --check` clean for slates-merge and slates-server. The census over 128 histories: 75 copies accepted,
    354 refused, 9 from the root, 40 from an older version, 24 into their own source's tree, 14 from a version their
    source changed after, 25 keys changed beneath a copy, 28 replaces, 31 nested pairs (2 with an inner replace), 19
    moves.
  - `cargo test --locked -p slates-server --lib` at `4ff0b7131`: 590 passed, 3 ignored, 1 failed:
    `deadlines::tests::a_status_capture_is_held_the_failover_delay_from_its_capture_not_its_request`. It fails 5 of 5
    alone and 3 of 3 with main's `verbs.rs` put back (main `d449a10c7`), so it is main's, not this change's (reported).
  - Red seven ways (r1040, 06:42 to 06:48 UTC), each on a copy of the tree with one guard taken out, the whole `--lib`
    suite run: copies of a changed source refused again, 4 failed; no cuts, 2; no empty start, 2; the fix left out of
    the entry, 4; replay reading the head's bytes, 1; the verdict reading the head's bytes, 1; no room, 1.
  - Main's: the integration suites, clippy and `cargo xtask check` (the run-request queue).
- **Owed** (A-626's later parts): a checkpoint of a green holding a copy (part 3: its copies and the identity's sum
  carried); a rebase restating a copy (part 4: the deriver's `VolumeOp::Copy`).
