# A Subversion copy refused a stored source whose landed window was let go (2026-11-22)

## Description
codex-review, at e7269dae4 on the MacBook (arm64), routed by main 01a20182
(`inbox/main-from-codex-review-svn-cold-copy.md` on the fan-out ledger). The setup is a daemon holding a store:
1. r1 adds `/trunk/a.txt` (19 bytes) and r2 changes it.
2. Another reference holds r1's text.
3. Its landed window is let go (`ContentStore::let_go_object_landed`).

The node's changeset store still answered all 19 bytes (`disk.text`). Copying `/trunk@1` to `/copy` answered
`Err(Unstored)`, not r3. Red: `repository::store_tests::a_subversion_copy_from_a_stored_revision_reads_a_window_that_was_let_go`
(`left: Err(Unstored)`, `right: Ok(3)`).

## Root cause
A commit brings back, before its step, every text it names that the records keep stored and the content store lacks
(`svn_commit::bring_texts`). "Lacks" was read as `!ContentStore::contains`. An object whose windows were let go keeps
its entry, and the store says so (`ContentStore::holds_whole`: held, and none of its windows let go). So nothing was
brought back. The commit's step then read the copy's source text (`read_named`), and the plane's read of a let-go window
answers nothing (`window_range_in`: an away window has no bytes here), so the copy was refused `Unstored`.

## Impact
Any client commit naming a stored text whose window was let go was refused, whether a copy from an older revision, a
revert, or a row naming a stored text. Nothing was corrupted; the retry was refused the same way while the window stayed
away.

## Exact edits
`crates/server/src/repository/svn_commit.rs`, `bring_texts`: a text is brought back when the content store does not hold
it whole (`holds_whole`), not when it has no entry. Bringing an object back that has an entry puts its windows again,
which brings back those let go and adds one hold. The caller gives that hold back after the step (`release_texts`), as
for any text brought.

## Siblings found, fixed in a second change (main's order of 2026-11-22)
Both asked `contains` where a reader needs every byte, and now ask `holds_whole`.
- `crates/server/src/vcs_store/svn_texts.rs` `named_lacking`, A-760's recovery pre-pass. A restored content store
  keeping a named text with one window away brought nothing back, and the green's replay was fenced `ContentMissing`.
  codex-review's red: `repository::store_tests::a_subversion_restart_replays_a_named_text_with_one_released_window`
  (the second repository fenced `ContentMissing { version: 1 }`; with it, the resident control
  `a_subversion_restart_replays_two_named_texts_whose_windows_are_resident`).
- `crates/server/src/vcs_store/svn_presenter.rs`, a hook view's bring-back before its files are written. A text held with
  its windows let go was taken as held, and reading the view's file was refused `ReferenceUnresolved`. Red:
  `vcs_store::svn_presenter::tests::a_hook_view_brings_back_a_stored_text_whose_windows_were_let_go`, over
  `test_store::a_text_whose_windows_are_away`.
