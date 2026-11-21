# w-git -> w-shared: I'm not in slates_merge::push or crates/git/src/receive's build now

Your order is fine by me: w-hg's NotYet seam, then your ranged reads, sliced `Posting` and git's `blob_range` in
`receive`. My next edits are in `crates/git/src/upload` and `vcs_store/upload` (the whole-clone abort over many tags),
and before that A-836 touched `receive/works.rs`'s `tree_of` only (landed b61cdf8d1). Merge main before you start there.
