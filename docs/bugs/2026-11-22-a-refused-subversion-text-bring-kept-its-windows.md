# A refused bring of a Subversion text kept the windows it had put (2026-11-22)

## Description
codex-review, at e7269dae4 on the MacBook (arm64), routed by main 01a20182
(`inbox/main-from-codex-review-svn-bring-room.md` on the fan-out ledger). The whole-text reader that brings a stored text
back from the node's changeset store (`vcs_store::svn_texts::bring_whole_in`) took a 262,145-byte text into its writer.
A pressure hold set before the object was finished made its last byte exceed admission. The finish answered false, but
the content store's use rose from 0 to 262,144 bytes, and the first window stayed readable with no reader holding it.
Red: `vcs_store::svn_texts::tests::a_refused_subversion_text_bring_returns_its_completed_windows`
(`left: (262144, true)`, `right: (0, false)`).

## Root cause
`ContentStore::finish_object` refuses with what was kept still held by the writer, for its caller to abandon (its
`# Errors`). `finish_brought` matched `Err(_) => false` and dropped the writer, so the windows it had put stayed held
and charged with no owner.

## Impact
Every refused bring kept its windows until the daemon stopped. Those were the windows of a whole text a copy, a revert,
a hook view or A-760's recovery pre-pass brought back from the node's store. A shard under pressure, the case that
refuses, lost room on each one.

## Exact edits
`crates/server/src/vcs_store/svn_texts.rs`, `finish_brought`: a refused finish abandons the writer
(`ContentStore::abandon_object`).

## Siblings (every caller of `finish_object`)
- `crates/hg/src/push/intake.rs:1276` (w-hg's): `.finish_object(&mut writing).map_err(...)?` returns and drops the
  writer on a refusal: the same leak. Reported to main and w-hg, not changed here.
- Already abandoning on refusal: `repository/svn_commit.rs` `store_step_from` (the writer stays in `Storing`, and
  `release_stored` abandons it), `vcs_store/svn_import/texts.rs` `seal`, git's `store/encoding.rs` and
  `store/staging.rs`. `vcs_store/named_objects.rs` and `crates/vcs` are tests.
