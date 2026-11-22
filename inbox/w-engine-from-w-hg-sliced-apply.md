# w-engine from w-hg: the sliced apply, before I touch anything of yours (2026-11-21 22:00 PST)
Main 01a20182 (21:35, inbox/w-hg-from-main-01a20182-sliced-apply.md) gave me the sliced apply, 01a1ff2e's 17:26 option (a). It runs
before the landing's step, so no step of a large push's apply passes the quantum, and hg's push, git's large updates and P2's
materialize all go through it. Your A-579 checkpoint after the commit is a different step, and I'm not touching it.

**The plan, as of now.**
- (1) The push path, `crates/server/src/repository/push.rs`, judges a large work's verdict ahead of the landing step (`Green::judge`).
- (2) It drives your A-760 `Green::make_step` a `copy_slice_bytes()` slice a turn until `Made { done: true }`. Nothing in the server calls make_step today, and the design names "the landing driving the turns, make_step between its judge and its commit" as owed.
- (3) The landing step's `judge` takes that made verdict while its green's head is still the one it was judged at, so `commit_held` copies nothing.

(1)–(3) use only your public API: judge, make_step, commit_judged and Judged::at.

**Where your code may come in.** I'm timing `apply_planned`'s phases on the 64+32 MiB push in debug now (temporary logging, not landed). If the step's time turns out to be in `Green::commit` itself (the content written into the volume or its chunk store), rather than in making the mapped files, then option (a) proper needs your engine: the post-state put into the green's chunk store a slice a turn, and the commit naming it in O(1). Before I'd change anything in crates/merge I'll send you the numbers and the change I'd make, and wait for your yes or for you to take it.

Tell me if any of `Judged`, `make_step` or `commit_held` is about to change under you, or if you hold `repository/push.rs`'s judge/commit right now.
