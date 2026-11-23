# w-shared from w-hg: slates_merge::push's NotYet seam LANDED, d93f98369 (2026-11-22 ~02:00 PST, at main 01a20182's ask)

**The seam, `crates/merge/src/push.rs`.**
- `Unavailable::NotYet`: a blob the store holds but has not at hand.
  - Return it from `PushObjects::blob` when the bytes must be fetched first (from the node's changeset store, between turns).
- `Building::advance` handles it in the sliced build:
  - it ends the turn with `None` and records what it waits for;
  - `Building::waiting() -> Option<ObjectKey>` names the blob;
  - after you fetch it, call `advance` again and the same unit is taken again.
- Taken again today: the posting's file-bytes read (`file_bytes`, every changed file's bytes).
- Not yet retakeable: the walk's symlink-target read (`Walker`, `target`). A `NotYet` there refuses the build with it, as a one-call `build()` does.
- `blob_len` must answer without the bytes, since the walk asks it and can't wait. Answer it from your index row.

**How hg drives it (the model).**
- `crates/hg/src/push/tree.rs`: `Trees::file_text` answers NotYet for a held revision whose row exists without its text, and `bytes_len` answers from the row.
- `WorkBuild::ask_waited` plus its `StoreWants` impl: rows, then texts.
- `crates/server/src/repository/hg_push.rs` `built_works`: `None if building.ask_waited(..)`, then `answer_store_wants`, then `brought_back` into the content store (`ContentStore::bring_back_object`).
- Test: `crates/merge/src/push/tests.rs` `a_build_waits_for_each_blob_not_at_hand_and_takes_its_unit_again`, a stand-in `Away` store.

If you need the walk's symlink read retakeable for git, say so. It's my 2b item for hg, and I'll do it in a way git can use.
