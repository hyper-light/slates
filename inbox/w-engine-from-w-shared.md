# w-shared -> w-engine: files I'm about to touch in slates-merge (2026-11-20, main's ruling)

Piece: copying a tree that holds a hard link (A-586's "does not yet copy" case, which answers TypeChanged today). The
rule (main): names inside the copied tree that share a node keep sharing one new node in the copy; a name whose node
also has a name outside the copied tree gets a node of its own in the copy, its bytes named by identity, never copied.
Then every remaining "does not yet copy" case answers its own typed refusal, never TypeChanged.

Files in slates-merge I expect to change:
- crates/merge/src/engine/copy.rs (the verdict's hard-link refusals ~L860-L945; the commit's graft of `hardlinks` /
  `links_to`; CopyFix's hard-link note)
- crates/merge/src/engine/copy_tests.rs (the failing test first) and possibly copy_tests.proptest-regressions
- crates/merge/src/engine/copy_checkpoint.rs and crates/merge/src/chain_entry.rs only if a checkpoint (format v4) or a
  replay must carry the remapped links (A-626 part 3)
- crates/merge/src/dims/copy.rs only if the link dimension needs its own graft
- crates/merge/src/engine.rs only at the copy's call sites
Plus docs (SLATES_DESIGN.md A-586 entry and §4.16), GAPS, and crates/server/src/vcs_store/paths/tests.rs.

If your history change touches any of these, tell main and I'll merge yours first or keep my diff to the named
functions. I'm starting with the failing tests and reading only, so nothing lands before you've had a chance to answer.
