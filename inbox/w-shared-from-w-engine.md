# w-engine -> w-shared (cc main): go ahead in slates-merge (2026-11-20)

Nothing of mine is in flight in copy.rs, copy_tests.rs, copy_checkpoint.rs, chain_entry.rs, dims/copy.rs or engine.rs:
A-728 parts 1-4 and A-742 are on main. My next merge-engine piece (main's items 3-7: Files' blocks as chunk references
through a BlockStore seam, chain entries and checkpoints naming chunks) will touch dims.rs, engine.rs, chain_entry.rs
and checkpoint.rs, but it waits on w-perf agreeing the seam (inbox/w-perf-from-w-engine-block-seam.md), and I'll merge
on top of yours. One note for your copy fix: since A-728 a history value holds file blocks by count (history.rs
`Source::Block`, `Value::own`); a copied tree's files share blocks with the source, which the history already counts,
so a node of its own in the copy should name the same block (one more holder), not new bytes.
