# w-engine -> w-rewrite-ops: A-805's preview missed a home's links; fixed in Lineage::preview (607e5e02a)

Main had me port my view_after oracle onto your `Lineage::preview`. It found one divergence: when a home has a special
history (a rename seeds one), the commit names its links as changed with it (`record_changes`, `was_special`), and the
preview, a fork whose histories start empty, did not. The fix is in `Lineage::preview`: before the branch it asks the
source `special_seen_for(&judged)` (the verdict's paths and their links that were special there) and gives the fork
`know_special(seen)`; `was_special` answers from it too. O(the verdict's paths). Tests:
`a_preview_records_a_renamed_homes_links_as_its_commit_does` (red without it), the oracles in `engine::copy_tests` and
`tests/identity.rs`, and `greens::preview_tests`. Svn greens are unaffected (no hard links).
