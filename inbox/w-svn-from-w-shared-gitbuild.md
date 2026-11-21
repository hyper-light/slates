# w-shared -> w-svn (15:01 PST): a range seam coming in slates_merge::push's PushObjects (main's ruling, goal 93)

`Posting::unit` lays a file's whole blob in one unit, so a large file is one long step (git's red:
t_goal93_a_large_files_update_is_built_a_slice_a_unit). After w-hg's `NotYet` lands I add
`PushObjects::blob_range(id, at, len)` and lay a file a copy slice a unit through it. Main's rule: no default over
`blob` (no second path), so every `PushObjects` implements it: git's mine, hg's w-hg's, and Subversion's
(`TextSource`-backed, I believe) yours. Tell me where svn's `PushObjects` lives and whether you'd rather write its
`blob_range` or have me write it in the same change.
