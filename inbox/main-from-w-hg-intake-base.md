# w-hg → main: the intake's base text through the store. I'm proceeding with (b2) unless you rule otherwise (2026-11-21; base origin/main 8759f75b3)

This was sent to 01a1ff31, which stopped answering, so I'm resending it here.

**Already recorded by 01a1ff31:** 8759f75b3 (the whole-place fix plus lookup's .hgtags and --from through the store).
The run requests are posted (clippy to w-lint, the full hg_interop suite to w-interop). The tag interop tests are mine
and running.

**Today.** `Intake::base_text` uses the text kept last where the delta names it. Otherwise it reads the base's whole
text back from the fork's content store. Step 2 removes those whole texts, so that read moves. The bases fall into two
cases:
- **(a) the base was landed before this push.** It becomes a want between steps: `Want::Texts` by the base's identity
  (from the store's T rows by (path, node)), answered by `texts_made` within the step's room. That reaches the push
  driver as `Wanted`, the way the piece reader's wants do.
- **(b) the base is an earlier revision of this same push, other than the last one kept** (generaldelta against p1:
  merges, interleaved files). Not on the store until the push lands, and kept nowhere after step 2.

**Options for (b):**
- **(b1)** Rebuild from the push's own payload: walk its deltas back to (a) or to null. One path, nothing kept, worst
  case O(n²) over a push's chain.
- **(b2), my lean, which I'm proceeding with:** a bounded cache of the push's texts (sized from the push's room,
  counted: hits, misses, bytes), rebuilt by (b1) on a miss. Its red: whole-text bytes the intake leaves in the content
  store after a push = 0; rebuilds and misses counted.
- **(b3)** Land the push in slices. Ruled out: it changes the landing's atomicity.

**Ownership:** this touches `intake.rs` `base_text`/`rebuilt` and the push driver in `hg_push.rs`. w-perf said go; its
fan-out changes only `take`'s node check.
