### A-626 part 1 — `Copy` replaces what its destination holds and nests in one increment: Subversion's replace with history, a second copy into a new branch and a move inside one, never refused (2026-11-19, g295 for UI goal 93; §4.16 "The state's maps" and "The verdict", §4.17.4 items 3 and 3.5 and piece 3; A-586, A-618, A-621, A-631, A-632)
Applied in the same change to: §4.16 (a status block, the verdict's sentence on `Copy`, the "Copies" bullet), §4.17.4
(a status block, piece 3), the A-586 entry (its owed list), GAPS (the §4.17.4 entry), `crates/merge/src/ops_doc.rs`
(`REPLACE`, `Op::replaces`), `crates/merge/src/engine.rs` (each copy's `CopySpec` in the resolved increment,
`Effect::CopyTree`'s flag, the commit as `apply_effect` over a `Staged` set with `remove_file_now` and A-618's
`drop_metadata`, the readers' `last_entry`, `head_identity_from_scratch` split, the replace's reservations),
`crates/merge/src/engine/copy.rs` (nesting, the replace's verdict and removal, every source held before any copy lands,
the readers' ties, the reservations), `crates/merge/src/engine/branch.rs` (the reverts), `crates/merge/src/dims.rs` and
`dims/copy.rs` (`hold_tree` and `graft_tree`, `Xattrs::iter_tree`), `crates/merge/src/chain_entry.rs`
(`Applied::CopyTree`'s flag, read from A-632's index), `crates/merge/src/engine/reanchor.rs`, and the tests
(`engine/copy_tests.rs`).
- **What was there.** A-586's `Copy` refused a destination that held anything (`CreateCreate`) and any second copy,
  move or link into a copied tree in the same increment (`TypeChanged`). A stock client sends both, and FSFS commits
  both: a replace with history is a changed path `R` with a copy source (`svn rm` and `svn cp` of one path in one
  commit: re-tagging a release, restoring a file from an older revision), and a branch made and edited in one commit
  carries copies beneath it (`svn cp ^/trunk ^/branches/b` with `svn mv` or `svn cp` inside it, which the editor drives
  as `add-file` with a copy source and `delete-entry`).
- **The flag.** `REPLACE`, the low bit of a `Copy`'s flags (`0x01`; no other kind sets it, so a document that never
  sets it means what it meant): the destination's tree is taken away first, then the source's tree is copied there.
  The removal is the flag's; the document's other operations on paths in the destination's tree land after the copy,
  on what it brought. The chain entry records nothing more: replay reads the flag back from the increment's own
  operation through A-632's one-pass index, as it reads the source's version.
- **The verdict.** A replace is judged as the removal of its destination's tree, as an unlink is judged, and as A-586's
  addition: refused when the tree changed after the base (`DeleteModify`, a delete against a change) or holds a hard
  link or another link's target (`TypeChanged`: Subversion has neither, and the reverse map of links would carry them
  through the removal). A copy whose destination lies in another copy's tree of the same increment (the innermost one
  holding it) is judged against what that copy brings there, its source's tree at the same place, which the outer
  copy's verdict holds unchanged since its version: a tree there takes it only as a replace (`CreateCreate`
  otherwise). Subversion's move inside a copied tree is such a copy and a removal of the moved name, which A-586's
  same-increment rule already judges against the copied tree. A rename or a hard link with an end in a copied tree stays
  refused, typed: Subversion has neither.
- **The commit.** Every source is held first, before any copy of the version changes a map, since FSFS copies from a
  revision root and never from the transaction (`libsvn_fs_fs/tree.c` `copy_helper` refuses a mutable source: "Copy
  from mutable tree not currently supported"). The copies then land in their destinations' order, an outer one before
  those inside its tree: for a replace its destination's tree is taken away key by key, then the held source is
  grafted. The removal walks each map beneath the destination once and records each key as its own removal records it:
  a file as an unlink (its value seeded where its history does not hold it, and `file_removed` for A-612's rule), an
  IPC node with its metadata, a directory, a symbolic link, then every mode and attribute left, through A-618's
  `drop_metadata`; each path joins the version's change index.
- **The readers.** A key's own last entry at a copy's version that records a removal reads as before the copy
  (`covered_since`), so a key the replace took away and the copy gave back reads as the copy's from that version, and by
  its own history before it. Of the copies one version made above a key, the nearest is the latest (`copy_above`).
- **The identity (A-202).** Unchanged: the state's entries plus, for each copy, its entry less its tree's. A replace's
  removals move the sum as removals do. A key an outer copy brought and an inner replace of the same version took away
  gives back the outer copy's entry for it, read from the copies above it (`copied_entries`).
- **Cost, and the shape weighed.** A replace costs O(the replaced tree): one walk of each map beneath the destination
  and one removal a key. That is what removing the tree key by key costs, and what Subversion's delete of the same tree
  costs a slates increment, which has no tree removal. A file whose value its history does not hold (unchanged since
  version 0, or since the copy that gave it) is recorded whole (W2's `Value::whole` copies its bytes), as any removal
  records it. Both are secured before the verdict: `history_reservation` adds each replaced file's latest bytes and
  each IPC node, and `changes_reservation` each path taken away, named at its destination, twice (at the version and
  as its last change), a replace inside another copy's tree counted where that copy's source holds it; a test holds
  each at least what the commit grew by. The other shape, the replaced tree held by reference in the copy's record,
  costs O(the paths) and copies no byte, but needs a second reader rule (a key with no history of its own, read before
  the replace, from the held tree, recursively through the copies before it), an identity term for each held tree, its
  retention counted without a walk, a branch that grafts it back and a checkpoint that lists it. It is owed if a
  measured re-tag's removal outweighs its copy (GAPS).
- **Three defects of A-586's branch, found by this oracle** (`Lineage::branch` at a version before a copy).
  1. A key of the cut tree that held, at the branch's version, the value the head holds was left cut: a replace that
     gives a tree back unchanged meets it, and so did a copy bringing a removed key's bytes back.
  2. A key's value at the version was read from its own history alone for IPC nodes, directories, modes, links and
     attributes, so a key an older copy (at or before the version) gave its value read as before that copy whenever its
     path was visited for another change.
  3. A copy's entry was given back only for keys changed after the version, so a key whose history ended before the
     copy over it (a copy's own destination, removed and then copied over) had its head entry exchanged out with
     nothing standing for it.

  Now every key the change index names is read through the copies above it, set back wherever its value differs from
  what the branch holds once its cuts are made, and given back each copy's entry when it has a history of its own. The
  algebra: the branch's sum is the head's less each later copy's entry, plus each later copy's tree entries, less each
  key's head entry, plus its entry at the version; a key the change index does not name holds at both ends what the
  copies over it stand for, so only the named keys move it.
- **Main's clippy findings, with it.** `head_identity_from_scratch` split (`add_state_entries`); branch.rs's array of
  last changes, whose type and length clippy flagged, went with the third defect's fix (a slice of `HasHistory`).
- **Tests** (`crates/merge/src/engine/copy_tests.rs`).
  - A replace takes its destination's tree away and copies the source there; the version before reads byte for byte;
    the change index names what it took; it replays from its encoded chain entry.
  - A key a replace took away and gave back, changed later, and a directory it took away, made again later, read at
    every version.
  - Copies nest in one increment: a second copy where the outer leaves nothing, a replace where it brings a tree, and a
    move inside it; replayed.
  - A replace's reservations cover what its commit adds and exceed a plain copy's.
  - Branches before and after a replace and nested copies; before a copy that brings a removed key back unchanged;
    through the copy that gave a key its value.
  - Every refusal, the green unchanged; among them a second copy onto a filled place, and replaces of a tree changed
    after the base, of one holding a hard link and of a link's target.
  - The oracle, rewritten: a model that states every version's tree by the design's rules (a reference green cannot,
    since plain operations state no replace), over generated histories of puts, takes, directories, single copies
    (replacing or not, from the head or an older version), nested pairs and moves. The green decides every step as the
    model does and reads as it at every version (bases, bytes, the paths the change index names since); its identity
    equals its recount and its change index's bytes theirs; no map refuses a copy; a copy that replaces nothing walks
    no map; its chain replays; a branch at every version reads as the model and hashes as the replay, and making it
    walks no map. Its steps are chosen by a weighted selector over plain tuples (`prop_oneof!`'s strategy is
    shared-ownership backed, R2).
- **Evidence** (the Mac Studio M3 Ultra, kiln, debug, fresh clones of the branch; r868 at 05:51 UTC, load 8.9; r893
  from 05:59 to 06:03 UTC, load 5.0 to 6.2; 2026-11-19).
  - `cargo test --locked -p slates-merge --lib`: 99 of 99 at `eddac8c74` (24.48 s), `cargo fmt --check` clean. The
    census over 128 histories: 85 copies accepted, 362 refused, 8 from the root, 33 from an older version, 25 into
    their own source's tree, 71 refused for a source changed since, 33 keys changed beneath a copy, 31 replaces over a
    tree that held something, 31 nested pairs (7 with an inner replace), 20 moves.
  - Red first: the new tests over main's implementation (the engine files of `0020d61`, the tests and `REPLACE` kept):
    7 failed, every A-626 test, and A-586's 10 passed; the same over the branch's first base (`6ce9e687d`, r851).
  - The first green run, before merging main (`fcffd15c4`, r851): 61 of 63, both failures the tests' own (the branch
    test's expected identity at version 1 was version 2's; the oracle counted the walk of a branch's base read, which
    walks by design). After merging main (`682479e9f`, r868): 95 of 97, the branch defects above.
  - Red seven ways at `eddac8c74` (r893), each on a copy of the tree with one guard taken out, the whole `--lib` suite
    run: the readers' tie rule, 2 failed; the nested verdict, 2; the replace's refusal of a tree changed after the
    base, 1; the replaced tree's modes and attributes, 4; the replace's reservation, 1; every source held before any
    copy lands, 1 (the oracle); branch.rs before its fix, 4.
  - Main's: the integration suites, clippy and `cargo xtask check` (the run-request queue).
- **Owed** (A-626's later parts):
  - a copy from a version its source changed after (part 2): the source's changes since set back at the destination,
    recorded in the chain entry so replay reads no folded history, reserved before the verdict, with room in the
    server's entry bound (`crates/server/src/verbs.rs`, computed from the increment alone);
  - a checkpoint of a green holding a copy (part 3): its copies and the identity's sum carried;
  - a rebase restating a copy (part 4): the deriver's `VolumeOp::Copy`.
