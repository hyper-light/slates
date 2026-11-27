# A bookmark pushed alone was made at its line's head, not at its changeset

2026-11-26, w-hg; main's hg_interop round at b8ba82ad3 (release, stock hg 7.2.4, the Studio, 09:41–10:00).

## Description

`stock_hg_pulls_by_a_tag_over_http_and_a_branch_is_made_from_the_tag` (`crates/cli/tests/hg_interop.rs`) does three
things:

1. It pushes `first`, the `.hgtags` commit naming `v1` at `first`, and `third`.
2. It runs `hg push -B mark` with `mark` at `first`, behind `default`'s head.
3. It runs `slates repository branch create r from-tag --from v1`.

Step 3 exited 1: `refused: Unsupported { feature: "a branch made at `v1`, changeset a272c15a…, which no line or bookmark of
the repository heads" }`.

The push was accepted, but `mark`'s green read `third`, the head of `default`, not `first`. So
`repository::hg_source` found no line or bookmark heading the tagged changeset, and refused.

## Root cause

A push that brings no changesets has no fork. `hg push -B` over changesets the server already holds is one.

- The push's driver makes its creates ahead of the landing's step (`hg_push::land_sliced`, goal 93's landing spin,
  fda5613a1 of 2026-11-23). `push::PreparedCreates::plan` takes each `Create`'s work out of the push's `changes` and makes
  the create with it.
- `hg_push::land` hands the prepared creates on only in its recorded arm (`push::push_hg`).
- Its two arms that record no revisions called `RepositoryLogs::append(changes, parts)`, which dropped the prepared
  creates. Those arms are: no fork, and a fork that kept nothing.
- The landing step then found no prepared create for `mark` and made it from the plan's step. That step's work had
  already been taken out, so `mark` was made a plain branch of `default`, at `default`'s head.

Mercurial's `hg_made::head_of` read that head truly. Neither it nor the A-929 owed lines are involved: `default` here is
the repository's root line, moved eagerly.

The probe (throwaway prints, removed):
- `mark_change` built a work from `third` (base head `3c852f3c…`) to `first` (`e7b1cec8…`).
- `prepare_into` committed it ahead as version 1 with 3 effects (`.hgtags` and `third` removed, the head set).
- At the landing step, `creates_made` was given `prepared=None` and prepared `mark` again with no work.

The log is kept as `~/.local/state/w-hg-logs/2026-11-26-bookmark-alone-probe.log`.

## Red

`a_bookmark_pushed_at_a_tagged_changeset_behind_its_lines_head_heads_it_and_a_branch_is_made_from_the_tag`
(`crates/server/src/vcs_store/hg_view/tests.rs`) runs on a stored shard:

1. It pushes the interop test's three changesets.
2. It pushes a lone `bookmarks` part with `mark` at `first` (`hg_push::tests::pushed_bookmark`).
3. It branches `from-tag` from `v1` through the verb's door (`hg_tags::dispatch_awaited`).

It expects `mark` heading `first`, and then `from-tag` made at `first`.

Before the fix it failed `` `mark` heads `first` ``: left `3c852f3c…` (`third`), right `e7b1cec8…` (`first`).

## Impact

The bug affects any `hg push -B` that makes a bookmark at a changeset that is not a line's head, over changesets the
server holds, and any changegroup push whose fork kept no revisions.

- The bookmark was made at its base line's head and read there: by `hg clone` and `hg bookmarks`, and by
  `branch create --from`.
- The client was told the push was accepted.
- A bookmark made at a line's head, which builds no work, was unaffected.

The r951 hang of the same interop test (11-24) came before C1's door fix of 11-25 and is a different bug.

## Exact edits

- `crates/server/src/repository/push.rs`: `push_unrecorded` is `push_holding` with no stored record and with the push's
  held-ahead objects and prepared creates, as `push_hg` has them.
- `crates/server/src/repository/hg_push.rs`: `land`'s two arms that record nothing call `push_unrecorded` with
  `held_ahead` and `creates`. `RepositoryLogs` is no longer imported there.
- `crates/server/src/repository/hg_push/tests.rs`: `pushed_bookmark`.
- `crates/server/src/vcs_store/hg_view/tests.rs`: the red above.

## Siblings

`PreparedCreates::plan` has one caller, `hg_push::land_sliced`, so no other tool's push takes works out ahead. Git's
receive-pack and svn's commit land through their own appends with no prepared creates.
