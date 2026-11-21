# w-hg → main: lookup's by-use tests landed (356b28d96); step 2 has more synchronous readers than the plan named (2026-11-21)

**Landed (origin/main 356b28d96):** the hg_interop tests against stock 7.2. They are `pull -r v1` over HTTP and SSH,
a batched `pull -r v1 -r tip` (one round of tags asks), and `branch create --from v1` (a bookmark at the tag). Both
tests pass, and `vcs_store.hg_lookup_store_asks` moves in each.

**(b2) is ruled and I'm building the intake's base text now**, (b1) at the core and the cache on top, A/B as you set.

**A scope finding for step 2.** Besides the intake's base, these code paths still read a pushed file's whole text
synchronously through the index (`IndexHeld::file_text` / `Indexed::file`):
1. **`push/tree.rs`**, used by `repository/hg_push.rs` and `vcs_store/hg_import.rs`. It builds the version's tree, and
   its files' bytes, from the pushed or imported revisions. This is the big one. It materializes the green from the
   texts, and it has to read through the store between steps before step 2 can drop any file text.
2. **`push/made.rs`**: a made changeset's parent file texts (`held.file_text(change.path, node)`).
3. **`push/verify.rs`**'s whole verification (`held.file_text`).
4. **`serve/indexed.rs` `Store::file`**: the served view's own reads, used by whole answers and `named_changegroup`'s
   default `file_entry`.

So step 2 (the intake keeps no whole file text) is safe only after (1)-(4) read through the store. Changesets' and
manifests' whole texts stay in the content store: every reader of manifests (named_changegroup, lookup's tags_file,
files_present) reads them synchronously, and R2's 4.3 GB is file texts.

**Proposed order:** the base text (b1/b2, now), then (1) tree.rs through the store (wants between the push's and the
import's materialization steps), then (2)-(4), then step 2 with its red (file-text bytes the intake leaves in the
content store = 0). Say if you want a different order.
