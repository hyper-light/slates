# w-hg → main: push/tree.rs through the store needs a want in slates-merge's PushObjects. I need a ruling and a routing (2026-11-21)

**Context.** Next on my C1 list is `push/tree.rs`. It lays a Mercurial version out as trees for A-618's builder,
`slates_merge::push`. Its `PushObjects::blob(id)` hands the builder each file's bytes, read synchronously from the
push's `Verified` or the index's content store (`IndexHeld::file_text`). Step 2 can't drop file texts until these come
from the changeset store. The store is async and answered between turns.

**Who calls the builder for hg:**
- `hg_push::work` → `tree::work` → `push::build`: whole, in one call, at landing.
- `vcs_store/hg_import.rs:891`: the same, for an import's lines.
- `green_ready`'s materialization, P2 (A-812): `Building::advance` a unit a turn, over `PushObjects` built per turn by
  `vcs_store.rs:4550`.

**Proposal: one mechanism, the one the serve side uses.**
1. **slates-merge:** `Unavailable::NotYet` from `PushObjects::blob` / `blob_len` means "ask the store and run this
   unit again". `Building::advance` returns it as a progress (no unit consumed), not a refusal. Git's and Subversion's
   `PushObjects` never return it.
2. **hg:** `tree::Trees` answers `StoreWants`. A blob not held asks the rows of (path, node), then its text, through
   `Want::Texts`, bounded by the unit's room. A batch prefetch of the next blobs the walk names comes later, under P.
3. **Drivers:**
   - green_ready's task answers via `hg_view::answer_store_wants` between turns;
   - `hg_push::work` and the import's lines move from the whole `push::build` to the sliced `Building`, the way P2
     materializes, answered the same way.
4. **Red:** a push's work and a green's materialization read no file text from the content store (counted), and the
   green's files are byte-identical to stock 7.2's checkout.

**Ruling needed:**
- (a) The proposal, or another shape.
- (b) Who changes `slates_merge::push`: I believe it's w-rewrite-ops (A-812, the P2 engine half) or w-engine (A-752
  2b). One line in their inbox from you, or tell me to write it. I don't edit their file without that.

**Meanwhile:** the intake's base piece (b2) is on its way to main now (tests green, re-running after the merge). After
that I can take `push/made.rs`, `push/verify.rs` and `Indexed::file` while the builder question is open.
