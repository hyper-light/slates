# A file base framed without its let-go block was asked again without end

- **Date:** found 2026-11-27 (main's 57k hg push hang: 3 of 3 builds at 71d56135a, 8d025f72e and 279688c64 hung, then
  1 in 8 runs after c6d97a069; the hung run's 3-second sample, `~/.local/state/main-01a20182/rounds/hang-control/sample-6-T-b.txt`);
  diagnosed and fixed the same day (w-perf, main's order).
- **Status:** fixed in `crates/hg/src/push/intake_patched.rs`.
- **Subsystem:** the hg push intake's file revisions taken a window a step (goal 93, C1 (2); A-767 C1's bring-back).
- **Severity:** a hang. The push's intake spun on one file revision for as long as the push lasted (57 minutes in r42),
  two asks of the node's changeset store a slice (12,964,755 of them), the hg client idle.

## Symptom

In 30 s of the hung daemon: `hg_push.intake_slices` +69,955, `vcs_store.hg_intake_base_asks` +139,910 (two a slice:
the rows and then the text of one base), `vcs_store.hg_text_requests` +69,955. The sample: the serving shard in
`hg_push::take_sliced` -> `Taking::slice` -> `advance_patched` -> `Taker::base_brought_back` ->
`held::bring_back_file`'s `ContentStore::bring_back_object` (231 of 404 samples), with `ask_base` beside it and the
store's owner in `texts_by_tool`.

## Root cause

`FileBase::stored` took a file delta's base as the fork held it when its revision began: its metadata block framed
(`\1\n`, the block, `\1\n`) by reading `ContentStore::windows(&block)`, and its bytes' length the row's length less that
framing. `Windows` stops at the first window let go (`Kept::Away` yields none). So where the base's landing had let its
block go before the revision began (a copy's or a rename's block, landed in an earlier segment), the framing came out
as the two markers alone and the bytes' length long by the block's length. The whole length still summed right, so
nothing noticed. Every read of the bytes then asked `ContentStore::run_at` at that wrong length, which refuses at
every offset, so the fill said the base was unread and the step asked the store for it. `base_brought_back` brought the
block and the bytes back, but `Patched::base` was the capture taken at the revision's begin, never made again, so the
next step read it the same way and asked again: two asks a slice, without end. The function's own doc promised "none
where it holds its block not whole", which the code never checked.

Why it depended on timing: the block must have been let go before the delta on it began, which depends on when its
segment landed between the intake's slices (the server lands segments after every slice); a commit's code layout moved
the slices' timing (main's bisect: 3 of 3 with 71d56135a, which changed no behaviour by two readings).

## Impact

A push whose file delta rests on a copied or renamed file's revision, landed and let go before the delta's revision
began, hung its intake. Every large Mercurial push of a history with copies risked it.

## Fix

1. `FileBase::Stored::whole`: the base is whole where its framing read is the block's every byte and its markers
   (`ContentStore::len_of(&block)`) and its bytes are held whole (`ContentStore::holds_whole`).
2. `advance_patched` asks for a base not whole before reading any of it, so nothing is read framed short.
3. `base_brought_back` makes the base again from the fork after the bring-back, its windows held now.
4. The once rule (main's of 2026-11-27): a base is asked once, and once more at most where its windows went again after
   it was brought back; a third ask is refused `UnknownDeltaBase`, since an ask made again without bound is this
   hang's shape.
5. Counted: `Intaken::base_store_reads` (bases brought back) and the new `Intaken::base_store_reasked` (asked again),
   reported `vcs.hg.push_base_store_reads` and `vcs.hg.push_base_store_reasked`.

Red: `push::intake::tests::a_file_delta_on_a_base_whose_block_was_let_go_is_taken_after_its_base_is_brought_back`
(file A with a metadata block taken into a fork, its block and bytes let go as its landing does, then B a delta on A
pushed, every ask answered with A's whole text, within a bound derived from the push's size): before, still stepping at
the bound, 16 steps and 11 asks (9,995 asks at a bound of 10,000 steps); now 12 steps, one ask, the base brought back
and asked again at most once.

## Siblings swept

- The manifests' `Taker::bring_in` and the changelog's and manifests' `log_texts_ready` take the text the store gives
  as a fresh object or as owned bytes: no capture of a framing made before the bring-back, and no metadata block. Not
  this shape. (`bring_in`'s own race, a row above an asked link let go between the ask and its use, was fixed as
  f925f47d4.)
- Changed (main's ruling of 2026-11-27 04:41, "make it loud"): `ContentStore::windows` yields a typed miss
  (`WindowMiss::Away`, `WindowMiss::Unread`) at the first window or list it cannot read, and nothing after, where it
  ended without a word; `ContentRefused::Missed` carries one. Every reader handles it: `hg` `render.rs` (the
  obsstore's markers, refused unreadable), `index.rs` (a revision's text none, `framed_text` and `object_text` too,
  where `framed_text` had dropped a let-go block window by `flat_map`), `serve/indexed.rs` (a marker refused, a lend
  or framed copy none), `import/record.rs` (`write_text` none, `window_bytes` none, the markers refused),
  `push/chain_read.rs` (`read_object` none), `push/intake_patched.rs` (a block window missed leaves the base not
  whole); `server` `vcs_store/commits.rs` (a blob refused `Unmade::Unread`), `vcs_store/homes.rs` (`read_here` not
  held); `vcs` `ContentStore::object_bytes` (none), `derived/blame.rs` (refused `Content(Missed)`); `git`
  `derived/tree.rs` (a blob id refused `Content(Missed)`), `store/push.rs` and `store/views.rs` (no lend; `stored_len`
  counts to the first miss). Reds:
  `content::tests::a_walk_over_an_object_whose_windows_were_let_go_misses_typed_never_short` (the walk empty before),
  `derived::blame::tests::t_goal93_a_file_whose_windows_were_let_go_is_refused_never_blamed_short` (blamed short
  before) and git's `t_goal93_a_file_whose_windows_were_let_go_is_refused_never_named_short` (a tree named over no
  bytes before); each red with the old silent stop put back.
