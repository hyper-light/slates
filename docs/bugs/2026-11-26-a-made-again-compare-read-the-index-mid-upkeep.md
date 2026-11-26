# A made-again compare read the index mid-upkeep, so a rebuilt store's index tables differed from the live one's

2026-11-26, w-git; main's ruling of the morning (a recovery leak or a test race, decided from the code and a red).

## Description

`vcs_store::commits::tests::t_a664_p3_made_commits_name_the_one_before_and_are_made_again_from_the_log` failed about 1
run in 20 to 30 under load (main's interleaved A/B: 5 in 100 at e09267304, before C3 (b), and 4 in 100 at 0deefa902,
after it, the failures clustered at the same moments in both arms; one failure in w-git's gate run of 2026-11-26). Its
compare, `vcs_store::tests::assert_made_again_whole`, makes the store again from the log (`vcs_store::rebuild`, the
recovery's own replay) and requires it equal to the live store: rows, refs, learned depth, and every content the shard's
content store holds with its holders. On a failure the rows, refs and holders were equal, and a few small contents
differed: the store made again held two the live store did not, and lacked one it held (+101 B).

The question was whether a restart keeps holds production let go (a recovery leak) or whether the compare read the two
stores at different points (a test race).

## Root cause

A test race: the compare read the repository's index at different points of its upkeep on the two sides.

Main's diagnostic run (agents/w-git-flake-debug at 5fcd46f74, its 150th run, load 22 to 35;
`~/.local/state/main-01a20182/rounds/flake-debug/run-150.log`) named the differing contents: the store made again held
two `Table`s of 523 and 531 B, one holder each, and the live store one other. `Table` contents are the tables of a
`slates_vcs::Index` (`ContentStore::put_tables`): here the git repository's index of object rows
(`slates_git::store::Repository::rows`).

The index is merged by its upkeep (`vcs_store::index_upkeep`), a table at a time, by a task of its own:

- on the live store, a fetch's first lookups report to the upkeep (`index_upkeep::looked_up`), and an index never merged
  is merged once (`compaction_due`, or `compactions == 0`), by `drive`, a detached task, a slice a step;
- on the store made again, the rebuild's end (`replay_ended`) begins a merge of every run it alone holds
  (`index_upkeep::built`), again by a detached task.

`assert_made_again_whole` runs in one step (inside one `on_state`), so neither task runs during it; but whether the live
store's merge had ended before it began depended on how the test's earlier awaits were scheduled. Under load the live
merge sometimes ended first: the live index read as one merged run's tables, the store made again as its unmerged runs'
tables, its merge's task never run by the compare. The index's rows (what a lookup answers) were equal either way.

Not a recovery leak: the store made again holds the tables of runs its own upkeep task merges once it runs, and gives
back as it merges them, as the live store's did. Nothing production let go is kept past that, and no record carries an
index's physical tables.

## Red

`vcs_store::tests::t_a664_a_store_made_again_compares_its_index_tables_merged_whole_on_both_sides`: push three commits
(the index three runs), merge the live index whole first (`index_upkeep::merged_whole`, the shape the flake met when the
fetch-begun merge had ended), then compare. With the compare as it was, it fails at `assert_made_again_whole`'s
`made == live` (21 contents, 5,377 B, in the store made again against 19, 5,175 B, live), deterministically.

## Fix

`assert_made_again_whole` reads each side's index merged whole: `index_upkeep::merged_whole` (`#[cfg(test)]`) drives a
merge under way to its end, then begins a merge of every run the index alone holds (a no-op below two runs) and drives it
to its end, on the live store before its facts and on the store made again after its rebuild. The red passes, its
`vcs_store.index_upkeep.merged` counter moving by one on each side (the non-vacuity count: both merges ran).

## What it does not change

Production: no product code changed. The upkeep's timing is the rule's (`slates_vcs::rule`), and a store made again
merges its index by its own task as before. The compare now judges the index by its rows and its merged tables, so a
physical-shape difference that upkeep would erase no longer fails it, while a row or a hold that differs still does.
